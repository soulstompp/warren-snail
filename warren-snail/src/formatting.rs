//! The formatting functions, as PostgreSQL 19's formatting.c reads and writes them under the
//! settings the snail answers under (lc_numeric and lc_time C, lc_monetary the one the file's money
//! was written under, TimeZone UTC, DateStyle ISO, MDY): to_char of the integers, the floats,
//! numeric, timestamp, timestamptz and interval, to_number, to_date and to_timestamp of a text and
//! its template.
//!
//! A date is held as its days since 1970-01-01, a timestamptz as its microseconds since
//! 1970-01-01 UTC, as `times.rs` holds them; the templates are read from their bytes as
//! formatting.c reads them.

use crate::monetary::Conv;
use crate::numeric::Numeric;
use crate::times::Interval;
use crate::{Error, Result};

/// The Julian day of 2000-01-01, where PostgreSQL counts its dates and instants from.
const POSTGRES_EPOCH_JDATE: i64 = 2_451_545;
/// The Julian day of 1970-01-01.
const UNIX_EPOCH_JDATE: i64 = 2_440_588;
const USECS_PER_DAY: i64 = 86_400_000_000;
const USECS_PER_HOUR: i64 = 3_600_000_000;
const USECS_PER_MINUTE: i64 = 60_000_000;
const USECS_PER_SEC: i64 = 1_000_000;
/// Microseconds from 1970-01-01 to 2000-01-01.
const EPOCH_SHIFT: i64 = (POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) * USECS_PER_DAY;
/// PostgreSQL's first instant and the first past its last, counted from 2000-01-01.
const MIN_TIMESTAMP: i64 = -211_813_488_000_000_000;
const END_TIMESTAMP: i64 = 9_223_371_331_200_000_000;
/// The first Julian day past a date.
const DATE_END_JULIAN: i64 = 2_147_483_494;
const MAX_TZDISP_HOUR: i32 = 15;
const DCH_MAX_ITEM_SIZ: usize = 12;
const NUM_MAX_ITEM_SIZ: usize = 8;
const MAX_ROMAN_LEN: usize = 15;
const TOKMAXLEN: usize = 10;
/// The digits a real and a double precision print at most.
const FLT_DIG: i32 = 6;
const DBL_DIG: i32 = 15;

const MONTHS_FULL: [&str; 12] = [
    "January",
    "February",
    "March",
    "April",
    "May",
    "June",
    "July",
    "August",
    "September",
    "October",
    "November",
    "December",
];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];
const DAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const DAYS_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const ADBC_STRINGS: [&str; 4] = ["ad", "bc", "AD", "BC"];
const ADBC_STRINGS_LONG: [&str; 4] = ["a.d.", "b.c.", "A.D.", "B.C."];
const AMPM_STRINGS: [&str; 4] = ["am", "pm", "AM", "PM"];
const AMPM_STRINGS_LONG: [&str; 4] = ["a.m.", "p.m.", "A.M.", "P.M."];
const RM_MONTHS_UPPER: [&str; 12] = [
    "XII", "XI", "X", "IX", "VIII", "VII", "VI", "V", "IV", "III", "II", "I",
];
const RM_MONTHS_LOWER: [&str; 12] = [
    "xii", "xi", "x", "ix", "viii", "vii", "vi", "v", "iv", "iii", "ii", "i",
];
const RM1: [&str; 9] = ["I", "II", "III", "IV", "V", "VI", "VII", "VIII", "IX"];
const RM10: [&str; 9] = ["X", "XX", "XXX", "XL", "L", "LX", "LXX", "LXXX", "XC"];
const RM100: [&str; 9] = ["C", "CC", "CCC", "CD", "D", "DC", "DCC", "DCCC", "CM"];
const NUM_TH_UPPER: [&str; 4] = ["ST", "ND", "RD", "TH"];
const NUM_TH_LOWER: [&str; 4] = ["st", "nd", "rd", "th"];
const DAY_TAB: [[i32; 12]; 2] = [
    [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
    [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31],
];

/// The zone abbreviations of UTC, read at offset 0, and every other of PostgreSQL's `Default`
/// set, which the snail refuses by name.
const UTC_ABBREVIATIONS: &[&str] = &["gmt", "uct", "ut", "utc", "z", "zulu"];
const OTHER_ABBREVIATIONS: &[&str] = &[
    "acdt", "acsst", "acst", "act", "acwst", "adt", "aedt", "aesst", "aest", "aft", "akdt", "akst",
    "almst", "almt", "amst", "amt", "anast", "anat", "arst", "art", "ast", "awsst", "awst",
    "azost", "azot", "azst", "azt", "bdst", "bdt", "bnt", "bort", "bot", "bra", "brst", "brt",
    "bst", "btt", "cadt", "cast", "cct", "cdt", "cest", "cet", "cetdst", "chadt", "chast", "chut",
    "ckt", "clst", "clt", "cot", "cst", "cxt", "davt", "ddut", "easst", "east", "eat", "edt",
    "eest", "eet", "eetdst", "egst", "egt", "est", "fet", "fjst", "fjt", "fkst", "fkt", "fnst",
    "fnt", "galt", "gamt", "gest", "get", "gft", "gilt", "gyt", "hkt", "hst", "ict", "idt", "iot",
    "irkst", "irkt", "irt", "ist", "jayt", "jst", "kdt", "kgst", "kgt", "kost", "krast", "krat",
    "kst", "lhdt", "lhst", "ligt", "lint", "lkt", "magst", "magt", "mart", "mawt", "mdt", "mest",
    "mesz", "met", "metdst", "mez", "mht", "mmt", "mpt", "msd", "msk", "mst", "must", "mut", "mvt",
    "myt", "nddt", "ndt", "nft", "novst", "novt", "npt", "nst", "nut", "nzdt", "nzst", "nzt",
    "omsst", "omst", "pdt", "pet", "petst", "pett", "pgt", "pht", "pkst", "pkt", "pmdt", "pmst",
    "pont", "pst", "pwt", "pyst", "pyt", "ret", "sadt", "sast", "sct", "sgt", "taht", "tft", "tjt",
    "tkt", "tmt", "tot", "trut", "tvt", "ulast", "ulat", "uyst", "uyt", "uzst", "uzt", "vet",
    "vlast", "vlat", "volt", "vut", "wadt", "wakt", "wast", "wat", "wdt", "wet", "wetdst", "wft",
    "wgst", "wgt", "xjt", "yakst", "yakt", "yapt", "yekst", "yekt",
];

fn err(msg: impl Into<String>) -> Error {
    Error::new(msg.into())
}

/// C's `isspace` under the C locale.
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r')
}

/// The bytes of the character a UTF-8 lead byte begins, as `pg_utf_mblen` counts them.
fn mblen(b: u8) -> usize {
    if b & 0x80 == 0 {
        1
    } else if b & 0xe0 == 0xc0 {
        2
    } else if b & 0xf0 == 0xe0 {
        3
    } else if b & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

/// A byte of a string, NUL past its end, as a C string reads.
fn at(s: &[u8], i: usize) -> u8 {
    s.get(i).copied().unwrap_or(0)
}

/// The bytes from `i` to the string's first NUL, as a C string.
fn cstr(s: &[u8], i: usize) -> &[u8] {
    let rest = s.get(i..).unwrap_or(&[]);
    match rest.iter().position(|&b| b == 0) {
        Some(n) => &rest[..n],
        None => rest,
    }
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

// ---------------------------------------------------------------------------------------------
// The template's keywords

/// The keywords a template names, as formatting.c's DCH_poz and NUM_poz number them.
#[allow(non_camel_case_types, clippy::upper_case_acronyms)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum K {
    A_D,
    A_M,
    AD,
    AM,
    B_C,
    BC,
    CC,
    DAY,
    DDD,
    DD,
    DY,
    Day,
    Dy,
    D,
    FF1,
    FF2,
    FF3,
    FF4,
    FF5,
    FF6,
    FX,
    HH24,
    HH12,
    HH,
    IDDD,
    ID,
    IW,
    IYYY,
    IYY,
    IY,
    I,
    J,
    MI,
    MM,
    MONTH,
    MON,
    MS,
    Month,
    Mon,
    OF,
    P_M,
    PM,
    Q,
    RM,
    SSSS,
    SS,
    TZH,
    TZM,
    TZ,
    US,
    WW,
    W,
    Y_YYY,
    YYYY,
    YYY,
    YY,
    Y,
    a_d,
    a_m,
    ad,
    am,
    b_c,
    bc,
    day,
    dy,
    month,
    mon,
    p_m,
    pm,
    rm,
    tz,
    // The number keywords.
    NComma,
    NDec,
    N0,
    N9,
    NB,
    NC,
    ND,
    NE,
    NFM,
    NG,
    NL,
    NMI,
    NPL,
    NPR,
    NRN,
    NSG,
    NSP,
    NS,
    NTH,
    NV,
    Nrn,
    Nth,
}

/// The date convention a keyword belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    None,
    Gregorian,
    IsoWeek,
}

#[derive(Debug, PartialEq, Eq)]
struct KeyWord {
    name: &'static str,
    id: K,
    is_digit: bool,
    mode: Mode,
}

const fn kw(name: &'static str, id: K, is_digit: bool, mode: Mode) -> KeyWord {
    KeyWord {
        name,
        id,
        is_digit,
        mode,
    }
}

use Mode::{Gregorian as G, IsoWeek as IW, None as NO};

/// DCH_keywords, in formatting.c's order: within a first letter, a longer keyword before one it
/// begins.
static DCH_KEYWORDS: [KeyWord; 112] = [
    kw("A.D.", K::A_D, false, NO),
    kw("A.M.", K::A_M, false, NO),
    kw("AD", K::AD, false, NO),
    kw("AM", K::AM, false, NO),
    kw("B.C.", K::B_C, false, NO),
    kw("BC", K::BC, false, NO),
    kw("CC", K::CC, true, NO),
    kw("DAY", K::DAY, false, NO),
    kw("DDD", K::DDD, true, G),
    kw("DD", K::DD, true, G),
    kw("DY", K::DY, false, NO),
    kw("Day", K::Day, false, NO),
    kw("Dy", K::Dy, false, NO),
    kw("D", K::D, true, G),
    kw("FF1", K::FF1, true, NO),
    kw("FF2", K::FF2, true, NO),
    kw("FF3", K::FF3, true, NO),
    kw("FF4", K::FF4, true, NO),
    kw("FF5", K::FF5, true, NO),
    kw("FF6", K::FF6, true, NO),
    kw("FX", K::FX, false, NO),
    kw("HH24", K::HH24, true, NO),
    kw("HH12", K::HH12, true, NO),
    kw("HH", K::HH, true, NO),
    kw("IDDD", K::IDDD, true, IW),
    kw("ID", K::ID, true, IW),
    kw("IW", K::IW, true, IW),
    kw("IYYY", K::IYYY, true, IW),
    kw("IYY", K::IYY, true, IW),
    kw("IY", K::IY, true, IW),
    kw("I", K::I, true, IW),
    kw("J", K::J, true, NO),
    kw("MI", K::MI, true, NO),
    kw("MM", K::MM, true, G),
    kw("MONTH", K::MONTH, false, G),
    kw("MON", K::MON, false, G),
    kw("MS", K::MS, true, NO),
    kw("Month", K::Month, false, G),
    kw("Mon", K::Mon, false, G),
    kw("OF", K::OF, false, NO),
    kw("P.M.", K::P_M, false, NO),
    kw("PM", K::PM, false, NO),
    kw("Q", K::Q, true, NO),
    kw("RM", K::RM, false, G),
    kw("SSSSS", K::SSSS, true, NO),
    kw("SSSS", K::SSSS, true, NO),
    kw("SS", K::SS, true, NO),
    kw("TZH", K::TZH, false, NO),
    kw("TZM", K::TZM, true, NO),
    kw("TZ", K::TZ, false, NO),
    kw("US", K::US, true, NO),
    kw("WW", K::WW, true, G),
    kw("W", K::W, true, G),
    kw("Y,YYY", K::Y_YYY, true, G),
    kw("YYYY", K::YYYY, true, G),
    kw("YYY", K::YYY, true, G),
    kw("YY", K::YY, true, G),
    kw("Y", K::Y, true, G),
    kw("a.d.", K::a_d, false, NO),
    kw("a.m.", K::a_m, false, NO),
    kw("ad", K::ad, false, NO),
    kw("am", K::am, false, NO),
    kw("b.c.", K::b_c, false, NO),
    kw("bc", K::bc, false, NO),
    kw("cc", K::CC, true, NO),
    kw("day", K::day, false, NO),
    kw("ddd", K::DDD, true, G),
    kw("dd", K::DD, true, G),
    kw("dy", K::dy, false, NO),
    kw("d", K::D, true, G),
    kw("ff1", K::FF1, true, NO),
    kw("ff2", K::FF2, true, NO),
    kw("ff3", K::FF3, true, NO),
    kw("ff4", K::FF4, true, NO),
    kw("ff5", K::FF5, true, NO),
    kw("ff6", K::FF6, true, NO),
    kw("fx", K::FX, false, NO),
    kw("hh24", K::HH24, true, NO),
    kw("hh12", K::HH12, true, NO),
    kw("hh", K::HH, true, NO),
    kw("iddd", K::IDDD, true, IW),
    kw("id", K::ID, true, IW),
    kw("iw", K::IW, true, IW),
    kw("iyyy", K::IYYY, true, IW),
    kw("iyy", K::IYY, true, IW),
    kw("iy", K::IY, true, IW),
    kw("i", K::I, true, IW),
    kw("j", K::J, true, NO),
    kw("mi", K::MI, true, NO),
    kw("mm", K::MM, true, G),
    kw("month", K::month, false, G),
    kw("mon", K::mon, false, G),
    kw("ms", K::MS, true, NO),
    kw("of", K::OF, false, NO),
    kw("p.m.", K::p_m, false, NO),
    kw("pm", K::pm, false, NO),
    kw("q", K::Q, true, NO),
    kw("rm", K::rm, false, G),
    kw("sssss", K::SSSS, true, NO),
    kw("ssss", K::SSSS, true, NO),
    kw("ss", K::SS, true, NO),
    kw("tzh", K::TZH, false, NO),
    kw("tzm", K::TZM, true, NO),
    kw("tz", K::tz, false, NO),
    kw("us", K::US, true, NO),
    kw("ww", K::WW, true, G),
    kw("w", K::W, true, G),
    kw("y,yyy", K::Y_YYY, true, G),
    kw("yyyy", K::YYYY, true, G),
    kw("yyy", K::YYY, true, G),
    kw("yy", K::YY, true, G),
    kw("y", K::Y, true, G),
];

/// NUM_keywords, in formatting.c's order.
static NUM_KEYWORDS: [KeyWord; 36] = [
    kw(",", K::NComma, false, NO),
    kw(".", K::NDec, false, NO),
    kw("0", K::N0, false, NO),
    kw("9", K::N9, false, NO),
    kw("B", K::NB, false, NO),
    kw("C", K::NC, false, NO),
    kw("D", K::ND, false, NO),
    kw("EEEE", K::NE, false, NO),
    kw("FM", K::NFM, false, NO),
    kw("G", K::NG, false, NO),
    kw("L", K::NL, false, NO),
    kw("MI", K::NMI, false, NO),
    kw("PL", K::NPL, false, NO),
    kw("PR", K::NPR, false, NO),
    kw("RN", K::NRN, false, NO),
    kw("SG", K::NSG, false, NO),
    kw("SP", K::NSP, false, NO),
    kw("S", K::NS, false, NO),
    kw("TH", K::NTH, false, NO),
    kw("V", K::NV, false, NO),
    kw("b", K::NB, false, NO),
    kw("c", K::NC, false, NO),
    kw("d", K::ND, false, NO),
    kw("eeee", K::NE, false, NO),
    kw("fm", K::NFM, false, NO),
    kw("g", K::NG, false, NO),
    kw("l", K::NL, false, NO),
    kw("mi", K::NMI, false, NO),
    kw("pl", K::NPL, false, NO),
    kw("pr", K::NPR, false, NO),
    kw("rn", K::Nrn, false, NO),
    kw("sg", K::NSG, false, NO),
    kw("sp", K::NSP, false, NO),
    kw("s", K::NS, false, NO),
    kw("th", K::Nth, false, NO),
    kw("v", K::NV, false, NO),
];

const SUFFIX_FM: u8 = 0x01;
const SUFFIX_TH: u8 = 0x02;
const SUFFIX_TH_LOWER: u8 = 0x04;
const SUFFIX_SP: u8 = 0x08;
const SUFFIX_TM: u8 = 0x10;

/// DCH_suff: each suffix, its code, and whether it comes before its keyword.
const DCH_SUFF: [(&str, u8, bool); 7] = [
    ("FM", SUFFIX_FM, true),
    ("fm", SUFFIX_FM, true),
    ("TM", SUFFIX_TM, true),
    ("tm", SUFFIX_TM, true),
    ("TH", SUFFIX_TH, false),
    ("th", SUFFIX_TH_LOWER, false),
    ("SP", SUFFIX_SP, false),
];

fn is_fm(s: u8) -> bool {
    s & SUFFIX_FM != 0
}

fn is_tm(s: u8) -> bool {
    s & SUFFIX_TM != 0
}

fn is_thth(s: u8) -> bool {
    s & (SUFFIX_TH | SUFFIX_TH_LOWER) != 0
}

fn th_upper(s: u8) -> bool {
    s & SUFFIX_TH != 0
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum NodeKind {
    Action(&'static KeyWord),
    Char,
    Separator,
    Space,
}

/// One node of a parsed template: a keyword with its suffixes, or a character.
#[derive(Clone, Debug)]
struct Node {
    kind: NodeKind,
    character: Vec<u8>,
    suffix: u8,
}

impl Node {
    fn key(&self) -> Option<&'static KeyWord> {
        match self.kind {
            NodeKind::Action(k) => Some(k),
            _ => None,
        }
    }
}

/// index_seq_search: the keyword the string begins with, the longer of two alike first.
fn index_seq_search(s: &[u8], table: &'static [KeyWord]) -> Option<&'static KeyWord> {
    let c = *s.first()?;
    if c <= b' ' || c >= b'~' {
        return None;
    }
    let start = table.iter().position(|k| k.name.as_bytes()[0] == c)?;
    table[start..]
        .iter()
        .take_while(|k| k.name.as_bytes()[0] == c)
        .find(|k| s.starts_with(k.name.as_bytes()))
}

fn suff_search(s: &[u8], prefix: bool) -> Option<(usize, u8)> {
    DCH_SUFF
        .iter()
        .find(|(name, _, pre)| *pre == prefix && s.starts_with(name.as_bytes()))
        .map(|(name, id, _)| (name.len(), *id))
}

/// An ASCII printable character that is no letter and no digit.
fn is_separator_char(b: u8) -> bool {
    b > 0x20 && b < 0x7f && !b.is_ascii_alphanumeric()
}

/// What a number template describes, as NUMDesc holds it.
#[derive(Clone, Copy, Debug, Default)]
struct NumDesc {
    pre: i32,
    post: i32,
    lsign: i32,
    flag: u32,
    pre_lsign_num: i32,
    multi: i32,
    zero_start: i32,
    zero_end: i32,
}

const NUM_F_DECIMAL: u32 = 1 << 1;
const NUM_F_LDECIMAL: u32 = 1 << 2;
const NUM_F_ZERO: u32 = 1 << 3;
const NUM_F_BLANK: u32 = 1 << 4;
const NUM_F_FILLMODE: u32 = 1 << 5;
const NUM_F_LSIGN: u32 = 1 << 6;
const NUM_F_BRACKET: u32 = 1 << 7;
const NUM_F_MINUS: u32 = 1 << 8;
const NUM_F_PLUS: u32 = 1 << 9;
const NUM_F_ROMAN: u32 = 1 << 10;
const NUM_F_MULTI: u32 = 1 << 11;
const NUM_F_PLUS_POST: u32 = 1 << 12;
const NUM_F_MINUS_POST: u32 = 1 << 13;
const NUM_F_EEEE: u32 = 1 << 14;

const NUM_LSIGN_PRE: i32 = -1;
const NUM_LSIGN_POST: i32 = 1;
const NUM_LSIGN_NONE: i32 = 0;

impl NumDesc {
    fn has(&self, f: u32) -> bool {
        self.flag & f != 0
    }
}

/// NUMDesc_prepare: one keyword of a number template taken into its description.
fn num_prepare(num: &mut NumDesc, k: &KeyWord) -> Result<()> {
    if num.has(NUM_F_EEEE) && k.id != K::NE {
        return Err(err("\"EEEE\" must be the last pattern used"));
    }
    match k.id {
        K::N9 => {
            if num.has(NUM_F_BRACKET) {
                return Err(err("\"9\" must be ahead of \"PR\""));
            }
            if num.has(NUM_F_MULTI) {
                num.multi += 1;
            } else if num.has(NUM_F_DECIMAL) {
                num.post += 1;
            } else {
                num.pre += 1;
            }
        }
        K::N0 => {
            if num.has(NUM_F_BRACKET) {
                return Err(err("\"0\" must be ahead of \"PR\""));
            }
            if !num.has(NUM_F_ZERO) && !num.has(NUM_F_DECIMAL) {
                num.flag |= NUM_F_ZERO;
                num.zero_start = num.pre + 1;
            }
            if !num.has(NUM_F_DECIMAL) {
                num.pre += 1;
            } else {
                num.post += 1;
            }
            num.zero_end = num.pre + num.post;
        }
        K::NB => {
            if num.pre == 0 && num.post == 0 && !num.has(NUM_F_ZERO) {
                num.flag |= NUM_F_BLANK;
            }
        }
        K::ND | K::NDec => {
            if k.id == K::ND {
                num.flag |= NUM_F_LDECIMAL;
            }
            if num.has(NUM_F_DECIMAL) {
                return Err(err("multiple decimal points"));
            }
            if num.has(NUM_F_MULTI) {
                return Err(err("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_DECIMAL;
        }
        K::NFM => num.flag |= NUM_F_FILLMODE,
        K::NS => {
            if num.has(NUM_F_LSIGN) {
                return Err(err("cannot use \"S\" twice"));
            }
            if num.has(NUM_F_PLUS) || num.has(NUM_F_MINUS) || num.has(NUM_F_BRACKET) {
                return Err(err(
                    "cannot use \"S\" and \"PL\"/\"MI\"/\"SG\"/\"PR\" together",
                ));
            }
            if !num.has(NUM_F_DECIMAL) {
                num.lsign = NUM_LSIGN_PRE;
                num.pre_lsign_num = num.pre;
                num.flag |= NUM_F_LSIGN;
            } else if num.lsign == NUM_LSIGN_NONE {
                num.lsign = NUM_LSIGN_POST;
                num.flag |= NUM_F_LSIGN;
            }
        }
        K::NMI => {
            if num.has(NUM_F_LSIGN) {
                return Err(err("cannot use \"S\" and \"MI\" together"));
            }
            num.flag |= NUM_F_MINUS;
            if num.has(NUM_F_DECIMAL) {
                num.flag |= NUM_F_MINUS_POST;
            }
        }
        K::NPL => {
            if num.has(NUM_F_LSIGN) {
                return Err(err("cannot use \"S\" and \"PL\" together"));
            }
            num.flag |= NUM_F_PLUS;
            if num.has(NUM_F_DECIMAL) {
                num.flag |= NUM_F_PLUS_POST;
            }
        }
        K::NSG => {
            if num.has(NUM_F_LSIGN) {
                return Err(err("cannot use \"S\" and \"SG\" together"));
            }
            num.flag |= NUM_F_MINUS | NUM_F_PLUS;
        }
        K::NPR => {
            if num.has(NUM_F_LSIGN) || num.has(NUM_F_PLUS) || num.has(NUM_F_MINUS) {
                return Err(err(
                    "cannot use \"PR\" and \"S\"/\"PL\"/\"MI\"/\"SG\" together",
                ));
            }
            num.flag |= NUM_F_BRACKET;
        }
        K::Nrn | K::NRN => {
            if num.has(NUM_F_ROMAN) {
                return Err(err("cannot use \"RN\" twice"));
            }
            num.flag |= NUM_F_ROMAN;
        }
        K::NV => {
            if num.has(NUM_F_DECIMAL) {
                return Err(err("cannot use \"V\" and decimal point together"));
            }
            num.flag |= NUM_F_MULTI;
        }
        K::NE => {
            if num.has(NUM_F_EEEE) {
                return Err(err("cannot use \"EEEE\" twice"));
            }
            if num.has(NUM_F_BLANK)
                || num.has(NUM_F_FILLMODE)
                || num.has(NUM_F_LSIGN)
                || num.has(NUM_F_BRACKET)
                || num.has(NUM_F_MINUS)
                || num.has(NUM_F_PLUS)
                || num.has(NUM_F_ROMAN)
                || num.has(NUM_F_MULTI)
            {
                return Err(err("\"EEEE\" is incompatible with other formats"));
            }
            num.flag |= NUM_F_EEEE;
        }
        _ => {}
    }
    if num.has(NUM_F_ROMAN) && (num.flag & !(NUM_F_ROMAN | NUM_F_FILLMODE)) != 0 {
        return Err(err("\"RN\" is incompatible with other formats"));
    }
    Ok(())
}

/// parse_format: a template's nodes; for a number template, its description too.
fn parse_format(s: &[u8], dch: bool, num: Option<&mut NumDesc>) -> Result<Vec<Node>> {
    let table: &'static [KeyWord] = if dch { &DCH_KEYWORDS } else { &NUM_KEYWORDS };
    let mut num = num;
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let mut suffix = 0u8;
        if dch {
            if let Some((len, id)) = suff_search(&s[i..], true) {
                suffix |= id;
                i += len;
            }
        }
        if i < s.len() {
            if let Some(k) = index_seq_search(&s[i..], table) {
                i += k.name.len();
                if let Some(n) = num.as_deref_mut() {
                    num_prepare(n, k)?;
                }
                if dch && i < s.len() {
                    if let Some((len, id)) = suff_search(&s[i..], false) {
                        suffix |= id;
                        i += len;
                    }
                }
                out.push(Node {
                    kind: NodeKind::Action(k),
                    character: Vec::new(),
                    suffix,
                });
                continue;
            }
        }
        if i >= s.len() {
            break;
        }
        if s[i] == b'"' {
            i += 1;
            while i < s.len() {
                if s[i] == b'"' {
                    i += 1;
                    break;
                }
                if s[i] == b'\\' && i + 1 < s.len() {
                    i += 1;
                }
                let len = mblen(s[i]).min(s.len() - i);
                out.push(Node {
                    kind: NodeKind::Char,
                    character: s[i..i + len].to_vec(),
                    suffix: 0,
                });
                i += len;
            }
        } else {
            if s[i] == b'\\' && at(s, i + 1) == b'"' {
                i += 1;
            }
            let len = mblen(s[i]).min(s.len() - i);
            let kind = if dch && is_separator_char(s[i]) {
                NodeKind::Separator
            } else if is_space(s[i]) {
                NodeKind::Space
            } else {
                NodeKind::Char
            };
            out.push(Node {
                kind,
                character: s[i..i + len].to_vec(),
                suffix: 0,
            });
            i += len;
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// The calendar, as PostgreSQL's C computes it in int

fn date2j(year: i32, month: i32, day: i32) -> i32 {
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

fn j2date(jd: i32) -> (i32, i32, i32) {
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

fn j2day(date: i32) -> i32 {
    let d = date.wrapping_add(1) % 7;
    if d < 0 {
        d + 7
    } else {
        d
    }
}

fn is_leap(y: i32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

fn valid_julian(y: i32, m: i32) -> bool {
    (y > -4713 || (y == -4713 && m >= 11)) && (y < 5_874_898 || (y == 5_874_898 && m < 6))
}

fn isoweek2j(year: i32, week: i32) -> i32 {
    let day4 = date2j(year, 1, 4);
    let day0 = j2day(day4.wrapping_sub(1));
    (week.wrapping_sub(1))
        .wrapping_mul(7)
        .wrapping_add(day4.wrapping_sub(day0))
}

fn date2isoweek(year: i32, mon: i32, mday: i32) -> i32 {
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = date2j(year.wrapping_sub(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
    }
    let mut week = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
    if week >= 52 {
        day4 = date2j(year.wrapping_add(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            week = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
        }
    }
    week
}

fn date2isoyear(year: i32, mon: i32, mday: i32) -> i32 {
    let mut year = year;
    let dayn = date2j(year, mon, mday);
    let mut day4 = date2j(year, 1, 4);
    let mut day0 = j2day(day4.wrapping_sub(1));
    if dayn < day4.wrapping_sub(day0) {
        day4 = date2j(year.wrapping_sub(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        year = year.wrapping_sub(1);
    }
    let week = dayn.wrapping_sub(day4.wrapping_sub(day0)) / 7 + 1;
    if week >= 52 {
        day4 = date2j(year.wrapping_add(1), 1, 4);
        day0 = j2day(day4.wrapping_sub(1));
        if dayn >= day4.wrapping_sub(day0) {
            year = year.wrapping_add(1);
        }
    }
    year
}

fn date2isoyearday(year: i32, mon: i32, mday: i32) -> i32 {
    date2j(year, mon, mday)
        .wrapping_sub(isoweek2j(date2isoyear(year, mon, mday), 1))
        .wrapping_add(1)
}

// ---------------------------------------------------------------------------------------------
// to_char of time

/// fmt_tm: a date and time taken apart, its hour wide enough for an interval's.
#[derive(Clone, Copy, Debug, Default)]
struct FmtTm {
    sec: i32,
    min: i32,
    hour: i64,
    mday: i32,
    mon: i32,
    year: i32,
    wday: i32,
    yday: i32,
    gmtoff: i64,
}

/// sprintf's `%0*d`: zero-padded to a width, the sign within it.
fn zpad(v: i64, width: usize) -> String {
    format!("{v:0width$}")
}

/// get_th: the ordinal suffix of a number's text.
fn get_th(num: &[u8], upper: bool) -> Result<&'static str> {
    let len = num.len();
    let mut last = *num.last().ok_or_else(|| err("\"\" is not a number"))?;
    if !last.is_ascii_digit() {
        return Err(err(format!("\"{}\" is not a number", text(num))));
    }
    if len > 1 && num[len - 2] == b'1' {
        last = 0;
    }
    let i = match last {
        b'1' => 0,
        b'2' => 1,
        b'3' => 2,
        _ => 3,
    };
    Ok(if upper {
        NUM_TH_UPPER[i]
    } else {
        NUM_TH_LOWER[i]
    })
}

fn numth(s: &mut String, suffix: u8) -> Result<()> {
    if is_thth(suffix) {
        let th = get_th(s.as_bytes(), th_upper(suffix))?;
        s.push_str(th);
    }
    Ok(())
}

fn asc_initcap(s: &str) -> String {
    let mut was_alnum = false;
    s.bytes()
        .map(|b| {
            let c = if was_alnum {
                b.to_ascii_lowercase()
            } else {
                b.to_ascii_uppercase()
            };
            was_alnum = c.is_ascii_alphanumeric();
            c as char
        })
        .collect()
}

fn invalid_for_interval(is_interval: bool) -> Result<()> {
    if is_interval {
        return Err(err("invalid format specification for an interval value"));
    }
    Ok(())
}

/// A localized name under TM, as long as PostgreSQL allows it to be.
fn localized(s: String, key: &KeyWord) -> Result<String> {
    if s.len() > (key.name.len() + 2) * DCH_MAX_ITEM_SIZ {
        return Err(err("localized string format value too long"));
    }
    Ok(s)
}

fn adjust_year(year: i32, is_interval: bool) -> i32 {
    if is_interval {
        year
    } else if year <= 0 {
        (year - 1).wrapping_neg()
    } else {
        year
    }
}

/// DCH_to_char: a date and time written as a template's nodes ask.
fn dch_to_char(
    nodes: &[Node],
    is_interval: bool,
    tm: &FmtTm,
    fsec: i32,
    tzn: Option<&str>,
) -> Result<String> {
    let mut out = String::new();
    for n in nodes {
        let Some(k) = n.key() else {
            out.push_str(&text(&n.character));
            continue;
        };
        let fm = is_fm(n.suffix);
        let pm = tm.hour % 24 >= 12;
        let num = |v: i64, width: usize, out: &mut String| -> Result<()> {
            let mut s = zpad(v, if fm { 0 } else { width });
            numth(&mut s, n.suffix)?;
            out.push_str(&s);
            Ok(())
        };
        match k.id {
            K::A_M | K::P_M => out.push_str(if pm { "P.M." } else { "A.M." }),
            K::AM | K::PM => out.push_str(if pm { "PM" } else { "AM" }),
            K::a_m | K::p_m => out.push_str(if pm { "p.m." } else { "a.m." }),
            K::am | K::pm => out.push_str(if pm { "pm" } else { "am" }),
            K::HH | K::HH12 => {
                let h = if tm.hour % 12 == 0 { 12 } else { tm.hour % 12 };
                num(h, if tm.hour >= 0 { 2 } else { 3 }, &mut out)?;
            }
            K::HH24 => num(tm.hour, if tm.hour >= 0 { 2 } else { 3 }, &mut out)?,
            K::MI => num(i64::from(tm.min), if tm.min >= 0 { 2 } else { 3 }, &mut out)?,
            K::SS => num(i64::from(tm.sec), if tm.sec >= 0 { 2 } else { 3 }, &mut out)?,
            K::FF1 | K::FF2 | K::FF3 | K::MS | K::FF4 | K::FF5 | K::FF6 | K::US => {
                let (div, width) = match k.id {
                    K::FF1 => (100_000, 1),
                    K::FF2 => (10_000, 2),
                    K::FF3 | K::MS => (1000, 3),
                    K::FF4 => (100, 4),
                    K::FF5 => (10, 5),
                    _ => (1, 6),
                };
                let mut s = zpad(i64::from(fsec / div), width);
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::SSSS => {
                let mut s =
                    (tm.hour * 3600 + i64::from(tm.min) * 60 + i64::from(tm.sec)).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::tz | K::TZ => {
                invalid_for_interval(is_interval)?;
                if let Some(z) = tzn {
                    let z = if k.id == K::tz {
                        z.to_ascii_lowercase()
                    } else {
                        z.to_string()
                    };
                    if z.len() > k.name.len() * DCH_MAX_ITEM_SIZ {
                        return Err(err("time zone format value too long"));
                    }
                    out.push_str(&z);
                }
            }
            K::TZH => {
                invalid_for_interval(is_interval)?;
                let off = tm.gmtoff as i32;
                out.push(if tm.gmtoff >= 0 { '+' } else { '-' });
                out.push_str(&zpad(i64::from(off.wrapping_abs() / 3600), 2));
            }
            K::TZM => {
                invalid_for_interval(is_interval)?;
                let off = tm.gmtoff as i32;
                out.push_str(&zpad(i64::from(off.wrapping_abs() % 3600 / 60), 2));
            }
            K::OF => {
                invalid_for_interval(is_interval)?;
                let off = tm.gmtoff as i32;
                out.push(if tm.gmtoff >= 0 { '+' } else { '-' });
                out.push_str(&zpad(
                    i64::from(off.wrapping_abs() / 3600),
                    if fm { 0 } else { 2 },
                ));
                if off.wrapping_abs() % 3600 != 0 {
                    out.push(':');
                    out.push_str(&zpad(i64::from(off.wrapping_abs() % 3600 / 60), 2));
                }
            }
            K::A_D | K::B_C => {
                invalid_for_interval(is_interval)?;
                out.push_str(if tm.year <= 0 { "B.C." } else { "A.D." });
            }
            K::AD | K::BC => {
                invalid_for_interval(is_interval)?;
                out.push_str(if tm.year <= 0 { "BC" } else { "AD" });
            }
            K::a_d | K::b_c => {
                invalid_for_interval(is_interval)?;
                out.push_str(if tm.year <= 0 { "b.c." } else { "a.d." });
            }
            K::ad | K::bc => {
                invalid_for_interval(is_interval)?;
                out.push_str(if tm.year <= 0 { "bc" } else { "ad" });
            }
            K::MONTH | K::Month | K::month | K::MON | K::Mon | K::mon => {
                invalid_for_interval(is_interval)?;
                if tm.mon == 0 {
                    continue;
                }
                let full = matches!(k.id, K::MONTH | K::Month | K::month);
                let name = if full {
                    MONTHS_FULL[(tm.mon - 1) as usize]
                } else {
                    MONTHS[(tm.mon - 1) as usize]
                };
                let cased = match k.id {
                    K::MONTH | K::MON => name.to_ascii_uppercase(),
                    K::month | K::mon => name.to_ascii_lowercase(),
                    _ => asc_initcap(name),
                };
                if is_tm(n.suffix) {
                    out.push_str(&localized(cased, k)?);
                } else if full && !fm {
                    out.push_str(&format!("{cased:<9}"));
                } else {
                    out.push_str(&cased);
                }
            }
            K::MM => num(i64::from(tm.mon), if tm.mon >= 0 { 2 } else { 3 }, &mut out)?,
            K::DAY | K::Day | K::day | K::DY | K::Dy | K::dy => {
                invalid_for_interval(is_interval)?;
                let full = matches!(k.id, K::DAY | K::Day | K::day);
                let w = tm.wday.rem_euclid(7) as usize;
                let name = if full { DAYS[w] } else { DAYS_SHORT[w] };
                let cased = match k.id {
                    K::DAY | K::DY => name.to_ascii_uppercase(),
                    K::day | K::dy => name.to_ascii_lowercase(),
                    _ => asc_initcap(name),
                };
                if is_tm(n.suffix) {
                    out.push_str(&localized(cased, k)?);
                } else if full && !fm {
                    out.push_str(&format!("{cased:<9}"));
                } else {
                    out.push_str(&cased);
                }
            }
            K::DDD | K::IDDD => {
                let v = if k.id == K::DDD {
                    tm.yday
                } else {
                    date2isoyearday(tm.year, tm.mon, tm.mday)
                };
                num(i64::from(v), 3, &mut out)?;
            }
            K::DD => num(i64::from(tm.mday), 2, &mut out)?,
            K::D => {
                invalid_for_interval(is_interval)?;
                let mut s = (tm.wday + 1).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::ID => {
                invalid_for_interval(is_interval)?;
                let mut s = (if tm.wday == 0 { 7 } else { tm.wday }).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::WW => num(i64::from((tm.yday - 1) / 7 + 1), 2, &mut out)?,
            K::IW => num(
                i64::from(date2isoweek(tm.year, tm.mon, tm.mday)),
                2,
                &mut out,
            )?,
            K::Q => {
                if tm.mon == 0 {
                    continue;
                }
                let mut s = ((tm.mon - 1) / 3 + 1).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::CC => {
                let i = if is_interval {
                    tm.year / 100
                } else if tm.year > 0 {
                    (tm.year - 1) / 100 + 1
                } else {
                    tm.year / 100 - 1
                };
                let mut s = if (-99..=99).contains(&i) {
                    zpad(
                        i64::from(i),
                        if fm {
                            0
                        } else if i >= 0 {
                            2
                        } else {
                            3
                        },
                    )
                } else {
                    i.to_string()
                };
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::Y_YYY => {
                let y = adjust_year(tm.year, is_interval);
                let i = y / 1000;
                let mut s = format!("{},{}", i, zpad(i64::from(y - i * 1000), 3));
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::YYYY | K::IYYY | K::YYY | K::IYY | K::YY | K::IY | K::Y | K::I => {
                let iso = matches!(k.id, K::IYYY | K::IYY | K::IY | K::I);
                let base = adjust_year(tm.year, is_interval);
                let y = if iso {
                    adjust_year(date2isoyear(tm.year, tm.mon, tm.mday), is_interval)
                } else {
                    base
                };
                let (v, width) = match k.id {
                    K::YYYY | K::IYYY => (y, if base >= 0 { 4 } else { 5 }),
                    K::YYY | K::IYY => (y % 1000, if base >= 0 { 3 } else { 4 }),
                    K::YY | K::IY => (y % 100, if base >= 0 { 2 } else { 3 }),
                    _ => (y % 10, 1),
                };
                let width = if fm && width != 1 { 0 } else { width };
                let mut s = zpad(i64::from(v), width);
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::RM | K::rm => {
                if tm.mon == 0 && tm.year == 0 {
                    continue;
                }
                let months = if k.id == K::RM {
                    &RM_MONTHS_UPPER
                } else {
                    &RM_MONTHS_LOWER
                };
                let mon = if tm.mon == 0 {
                    if tm.year >= 0 {
                        0
                    } else {
                        11
                    }
                } else if tm.mon < 0 {
                    -(tm.mon + 1)
                } else {
                    12 - tm.mon
                };
                let name = months[mon as usize];
                if fm {
                    out.push_str(name);
                } else {
                    out.push_str(&format!("{name:<4}"));
                }
            }
            K::W => {
                let mut s = ((tm.mday - 1) / 7 + 1).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            K::J => {
                let mut s = date2j(tm.year, tm.mon, tm.mday).to_string();
                numth(&mut s, n.suffix)?;
                out.push_str(&s);
            }
            _ => {}
        }
    }
    Ok(out)
}

/// timestamp2tm under UTC: an instant counted from 2000-01-01 taken apart, with its day of the week
/// and of the year.
fn instant_tm(pg: i64) -> Result<(FmtTm, i32)> {
    let date = pg.div_euclid(USECS_PER_DAY) + POSTGRES_EPOCH_JDATE;
    let time = pg.rem_euclid(USECS_PER_DAY);
    if date < 0 || date > i64::from(i32::MAX) {
        return Err(err("timestamp out of range"));
    }
    let (year, mon, mday) = j2date(date as i32);
    let thisdate = date2j(year, mon, mday);
    let tm = FmtTm {
        sec: (time % USECS_PER_MINUTE / USECS_PER_SEC) as i32,
        min: (time % USECS_PER_HOUR / USECS_PER_MINUTE) as i32,
        hour: time / USECS_PER_HOUR,
        mday,
        mon,
        year,
        wday: (thisdate + 1) % 7,
        yday: thisdate - date2j(year, 1, 1) + 1,
        gmtoff: 0,
    };
    Ok((tm, (time % USECS_PER_SEC) as i32))
}

fn instant_to_char(t: i64, fmt: &str, zoned: bool) -> Result<Option<String>> {
    if fmt.is_empty() || t == i64::MIN || t == i64::MAX {
        return Ok(None);
    }
    let pg = t
        .checked_sub(EPOCH_SHIFT)
        .ok_or_else(|| err("timestamp out of range"))?;
    let (tm, fsec) = instant_tm(pg)?;
    let nodes = parse_format(fmt.as_bytes(), true, None)?;
    dch_to_char(
        &nodes,
        false,
        &tm,
        fsec,
        if zoned { Some("UTC") } else { None },
    )
    .map(Some)
}

/// `to_char(timestamp, text)`; none where the template is empty or the instant infinite.
pub fn to_char_timestamp(t: i64, fmt: &str) -> Result<Option<String>> {
    instant_to_char(t, fmt, false)
}

/// `to_char(timestamptz, text)` under TimeZone UTC; none where the template is empty or the
/// instant infinite.
pub fn to_char_timestamptz(t: i64, fmt: &str) -> Result<Option<String>> {
    instant_to_char(t, fmt, true)
}

/// `to_char(interval, text)`; none where the template is empty or the interval infinite.
pub fn to_char_interval(v: Interval, fmt: &str) -> Result<Option<String>> {
    if fmt.is_empty() || !v.is_finite() {
        return Ok(None);
    }
    let mut time = v.micros;
    let hour = time / USECS_PER_HOUR;
    time -= hour * USECS_PER_HOUR;
    let min = time / USECS_PER_MINUTE;
    time -= min * USECS_PER_MINUTE;
    let sec = time / USECS_PER_SEC;
    time -= sec * USECS_PER_SEC;
    let year = v.months / 12;
    let mon = v.months % 12;
    let tm = FmtTm {
        sec: sec as i32,
        min: min as i32,
        hour,
        mday: v.days,
        mon,
        year,
        wday: 0,
        yday: year
            .wrapping_mul(12)
            .wrapping_add(mon)
            .wrapping_mul(30)
            .wrapping_add(v.days),
        gmtoff: 0,
    };
    let nodes = parse_format(fmt.as_bytes(), true, None)?;
    dch_to_char(&nodes, true, &tm, time as i32, None).map(Some)
}

// ---------------------------------------------------------------------------------------------
// to_date and to_timestamp

/// TmFromChar: the fields a template reads from a text.
#[derive(Clone, Debug, Default)]
struct TmFromChar {
    mode: Option<Mode>,
    hh: i32,
    pm: i32,
    mi: i32,
    ss: i32,
    ssss: i32,
    d: i32,
    dd: i32,
    ddd: i32,
    mm: i32,
    ms: i32,
    year: i32,
    bc: i32,
    ww: i32,
    w: i32,
    cc: i32,
    j: i32,
    us: i32,
    yysz: i32,
    clock_12_hour: bool,
    tzsign: i32,
    tzh: i32,
    tzm: i32,
    ff: i32,
    has_tz: bool,
    gmtoffset: i32,
}

fn set_mode(tmfc: &mut TmFromChar, mode: Mode) -> Result<()> {
    if mode != Mode::None {
        match tmfc.mode {
            None => tmfc.mode = Some(mode),
            Some(m) if m != mode => return Err(err("invalid combination of date conventions")),
            _ => {}
        }
    }
    Ok(())
}

fn set_int(dest: &mut i32, value: i32, key: &KeyWord) -> Result<()> {
    if *dest != 0 && *dest != value {
        return Err(err(format!(
            "conflicting values for \"{}\" field in formatting string",
            key.name
        )));
    }
    *dest = value;
    Ok(())
}

/// is_next_separator: whether the node after a field reads no digit.
fn is_next_separator(nodes: &[Node], i: usize) -> bool {
    let Some(n) = nodes.get(i) else {
        return false;
    };
    if n.key().is_some() && is_thth(n.suffix) {
        return true;
    }
    match nodes.get(i + 1) {
        None => true,
        Some(next) => match next.key() {
            Some(k) => !k.is_digit,
            None => !(next.character.len() == 1 && next.character[0].is_ascii_digit()),
        },
    }
}

/// C's `strtol` in base 10 from `at`: the number, where it stops (`at` itself where it read no
/// digit), and whether it passed a long.
fn strtol(s: &[u8], at_: usize) -> (i64, usize, bool) {
    let mut i = at_;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let digits = i;
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
    if i == digits {
        return (0, at_, false);
    }
    let v = if neg { -v } else { v };
    if over || v > i128::from(i64::MAX) || v < i128::from(i64::MIN) {
        return (if neg { i64::MIN } else { i64::MAX }, i, true);
    }
    (v as i64, i, false)
}

/// from_char_parse_int_len: an integer of the field's width, or as wide as it runs where the
/// field is in fill mode or a non-digit follows; the characters it took.
fn parse_int_len(
    dest: Option<&mut i32>,
    s: &[u8],
    pos: &mut usize,
    len: usize,
    nodes: &[Node],
    i: usize,
) -> Result<usize> {
    let node = &nodes[i];
    let key = node.key().unwrap();
    let init = *pos;
    while *pos < s.len() && is_space(s[*pos]) {
        *pos += 1;
    }
    let rest = cstr(s, *pos);
    let copy: &[u8] = &rest[..rest.len().min(len)];
    let used = copy.len();
    let (result, over) = if is_fm(node.suffix) || is_next_separator(nodes, i) {
        let (v, end, over) = strtol(s, init);
        *pos = end;
        (v, over)
    } else {
        if used < len {
            return Err(err(format!(
                "source string too short for \"{}\" formatting field",
                key.name
            )));
        }
        let (v, end, over) = strtol(copy, 0);
        let used = end;
        if used > 0 && used < len {
            return Err(err(format!(
                "invalid value \"{}\" for \"{}\"",
                text(copy),
                key.name
            )));
        }
        *pos += used;
        (v, over)
    };
    if *pos == init {
        return Err(err(format!(
            "invalid value \"{}\" for \"{}\"",
            text(copy),
            key.name
        )));
    }
    if over || result < i64::from(i32::MIN) || result > i64::from(i32::MAX) {
        return Err(err(format!(
            "value for \"{}\" in source string is out of range",
            key.name
        )));
    }
    if let Some(d) = dest {
        set_int(d, result as i32, key)?;
    }
    Ok(*pos - init)
}

/// seq_search_ascii: the entry the string begins with, compared without case; its index and the
/// bytes it matched.
fn seq_search_ascii(name: &[u8], array: &[&str]) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    let first = name[0].to_ascii_lowercase();
    for (idx, a) in array.iter().enumerate() {
        let a = a.as_bytes();
        if a[0].to_ascii_lowercase() != first {
            continue;
        }
        let mut p = 1;
        loop {
            if p == a.len() {
                return Some((idx, p));
            }
            if p >= name.len() {
                break;
            }
            if !a[p].eq_ignore_ascii_case(&name[p]) {
                break;
            }
            p += 1;
        }
    }
    None
}

/// The string in a casing of the database's default collation.
fn cased(s: &[u8], upper: bool) -> Vec<u8> {
    let t = String::from_utf8_lossy(s);
    crate::strings::case(&t, upper, crate::catalog::Casing::Libc)
        .map(String::into_bytes)
        .unwrap_or_else(|_| s.to_vec())
}

/// seq_search_localized: the entry a string begins with, case folded as the collation folds it.
fn seq_search_localized(name: &[u8], array: &[&str]) -> Option<(usize, usize)> {
    if name.is_empty() {
        return None;
    }
    for (idx, a) in array.iter().enumerate() {
        if name.starts_with(a.as_bytes()) {
            return Some((idx, a.len()));
        }
    }
    let lower_name = cased(&cased(name, true), false);
    let fold = |s: &[u8]| cased(&cased(s, true), false);
    for (idx, a) in array.iter().enumerate() {
        let element = fold(a.as_bytes());
        if !lower_name.starts_with(&element) {
            continue;
        }
        if lower_name.len() == element.len() {
            return Some((idx, name.len()));
        }
        let element_chars = String::from_utf8_lossy(&element).chars().count();
        let mut substr = 0;
        let mut chars = 0;
        while chars < element_chars && substr < name.len() {
            substr += mblen(name[substr]).min(name.len() - substr);
            chars += 1;
        }
        if fold(&name[..substr]) == element {
            return Some((idx, substr));
        }
        let mut substr = 0;
        while substr < name.len() {
            substr += mblen(name[substr]).min(name.len() - substr);
            if fold(&name[..substr]) == element {
                return Some((idx, substr));
            }
        }
    }
    None
}

/// from_char_seq_search: one of the field's words read from the text, its index.
fn seq_search(
    s: &[u8],
    pos: &mut usize,
    array: &[&str],
    localized: bool,
    key: &KeyWord,
) -> Result<i32> {
    let rest = cstr(s, *pos);
    let found = if localized {
        seq_search_localized(rest, array)
    } else {
        seq_search_ascii(rest, array)
    };
    match found {
        Some((idx, len)) if len > 0 => {
            *pos += len;
            Ok(idx as i32)
        }
        _ => {
            let copy: Vec<u8> = rest
                .iter()
                .copied()
                .take_while(|&b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
                .collect();
            Err(err(format!(
                "invalid value \"{}\" for \"{}\"",
                text(&copy),
                key.name
            )))
        }
    }
}

fn skip_thth(s: &[u8], pos: &mut usize, suffix: u8) {
    if is_thth(suffix) {
        for _ in 0..2 {
            if at(s, *pos) != 0 {
                *pos += mblen(s[*pos]).min(s.len() - *pos);
            }
        }
    }
}

fn adjust_partial_year_to_2020(year: i32) -> i32 {
    if year < 70 {
        year + 2000
    } else if year < 100 {
        year + 1900
    } else if year < 520 {
        year + 2000
    } else if year < 1000 {
        year + 1000
    } else {
        year
    }
}

/// DecodeTimezoneAbbrevPrefix under TimeZone UTC: the longest abbreviation the text begins with,
/// UTC's read at offset 0, any other of PostgreSQL's refused by name; its length, 0 for none.
fn timezone_abbrev_prefix(s: &[u8]) -> Result<usize> {
    let low: Vec<u8> = s
        .iter()
        .take(TOKMAXLEN)
        .take_while(|b| b.is_ascii_alphabetic())
        .map(|b| b.to_ascii_lowercase())
        .collect();
    for len in (1..=low.len()).rev() {
        let key = &low[..len];
        if UTC_ABBREVIATIONS.iter().any(|t| t.as_bytes() == key) {
            return Ok(len);
        }
        if OTHER_ABBREVIATIONS.iter().any(|t| t.as_bytes() == key) {
            return Err(err(format!(
                "time zone abbreviation \"{}\" is not read: the snail reads only the \
                 abbreviations of UTC",
                text(key)
            )));
        }
    }
    Ok(0)
}

/// sscanf's `%d,%03d%n`: the two numbers and the characters taken, where both are read.
fn scan_y_yyy(s: &[u8]) -> Option<(i64, i64, usize)> {
    let (a, mut i, _) = strtol(s, 0);
    if i == 0 {
        return None;
    }
    if at(s, i) != b',' {
        return None;
    }
    i += 1;
    while i < s.len() && is_space(s[i]) {
        i += 1;
    }
    let start = i;
    let mut neg = false;
    if i < s.len() && (s[i] == b'+' || s[i] == b'-') {
        neg = s[i] == b'-';
        i += 1;
    }
    let mut v: i64 = 0;
    let mut digits = 0;
    while i < s.len() && i - start < 3 && s[i].is_ascii_digit() {
        v = v * 10 + i64::from(s[i] - b'0');
        digits += 1;
        i += 1;
    }
    if digits == 0 {
        return None;
    }
    Some((a, if neg { -v } else { v }, i))
}

/// DCH_from_char: a text read as a template's nodes ask.
fn dch_from_char(nodes: &[Node], s: &[u8]) -> Result<TmFromChar> {
    let mut out = TmFromChar::default();
    let mut fx_mode = false;
    let mut extra_skip: i32 = 0;
    let mut pos = 0usize;
    let mut i = 0usize;
    while i < nodes.len() && at(s, pos) != 0 {
        let n = &nodes[i];
        let key = n.key();
        if !fx_mode && key.is_none_or(|k| k.id != K::FX) && (key.is_some() || i == 0) {
            while at(s, pos) != 0 && is_space(s[pos]) {
                pos += 1;
                extra_skip += 1;
            }
        }
        match n.kind {
            NodeKind::Space | NodeKind::Separator => {
                if !fx_mode {
                    extra_skip -= 1;
                    let c = at(s, pos);
                    if is_space(c) || is_separator_char(c) {
                        pos += 1;
                        extra_skip += 1;
                    }
                } else if at(s, pos) != 0 {
                    pos += mblen(s[pos]);
                }
                i += 1;
                continue;
            }
            NodeKind::Char => {
                if !fx_mode {
                    if extra_skip > 0 {
                        extra_skip -= 1;
                    } else if at(s, pos) != 0 {
                        pos += mblen(s[pos]);
                    }
                } else if at(s, pos) != 0 {
                    pos += mblen(s[pos]);
                }
                i += 1;
                continue;
            }
            NodeKind::Action(_) => {}
        }
        let k = key.unwrap();
        set_mode(&mut out, k.mode)?;
        let tm_suffix = is_tm(n.suffix);
        match k.id {
            K::FX => fx_mode = true,
            K::A_M | K::P_M | K::a_m | K::p_m => {
                let v = seq_search(s, &mut pos, &AMPM_STRINGS_LONG, false, k)?;
                set_int(&mut out.pm, v % 2, k)?;
                out.clock_12_hour = true;
            }
            K::AM | K::PM | K::am | K::pm => {
                let v = seq_search(s, &mut pos, &AMPM_STRINGS, false, k)?;
                set_int(&mut out.pm, v % 2, k)?;
                out.clock_12_hour = true;
            }
            K::HH | K::HH12 => {
                parse_int_len(Some(&mut out.hh), s, &mut pos, 2, nodes, i)?;
                out.clock_12_hour = true;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::HH24 => {
                parse_int_len(Some(&mut out.hh), s, &mut pos, 2, nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::MI => {
                parse_int_len(Some(&mut out.mi), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::SS => {
                parse_int_len(Some(&mut out.ss), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::MS => {
                let len = parse_int_len(Some(&mut out.ms), s, &mut pos, 3, nodes, i)?;
                out.ms = out.ms.wrapping_mul(match len {
                    1 => 100,
                    2 => 10,
                    _ => 1,
                });
                skip_thth(s, &mut pos, n.suffix);
            }
            K::FF1 | K::FF2 | K::FF3 | K::FF4 | K::FF5 | K::FF6 | K::US => {
                let width = match k.id {
                    K::FF1 => 1,
                    K::FF2 => 2,
                    K::FF3 => 3,
                    K::FF4 => 4,
                    K::FF5 => 5,
                    K::FF6 => 6,
                    _ => 0,
                };
                if width > 0 {
                    out.ff = width;
                }
                let len = parse_int_len(
                    Some(&mut out.us),
                    s,
                    &mut pos,
                    if k.id == K::US { 6 } else { out.ff as usize },
                    nodes,
                    i,
                )?;
                out.us = out.us.wrapping_mul(match len {
                    1 => 100_000,
                    2 => 10_000,
                    3 => 1000,
                    4 => 100,
                    5 => 10,
                    _ => 1,
                });
                skip_thth(s, &mut pos, n.suffix);
            }
            K::SSSS => {
                parse_int_len(Some(&mut out.ssss), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::tz | K::TZ | K::OF => {
                let mut as_of = k.id == K::OF;
                if !as_of {
                    let len = timezone_abbrev_prefix(cstr(s, pos))?;
                    if len > 0 {
                        out.has_tz = true;
                        out.gmtoffset = 0;
                        out.tzsign = 0;
                        pos += len;
                    } else if at(s, pos).is_ascii_alphabetic() {
                        return Err(err(format!(
                            "invalid value \"{}\" for \"{}\"",
                            text(cstr(s, pos)),
                            k.name
                        )));
                    } else {
                        as_of = true;
                    }
                }
                if as_of {
                    let c = at(s, pos);
                    if c == b'+' || c == b'-' || c == b' ' {
                        out.tzsign = if c == b'-' { -1 } else { 1 };
                        pos += 1;
                    } else if extra_skip > 0 && pos > 0 && s[pos - 1] == b'-' {
                        out.tzsign = -1;
                    } else {
                        out.tzsign = 1;
                    }
                    parse_int_len(Some(&mut out.tzh), s, &mut pos, 2, nodes, i)?;
                    if at(s, pos) == b':' {
                        pos += 1;
                        parse_int_len(Some(&mut out.tzm), s, &mut pos, 2, nodes, i)?;
                    }
                }
            }
            K::TZH => {
                let c = at(s, pos);
                if c == b'+' || c == b'-' || c == b' ' {
                    out.tzsign = if c == b'-' { -1 } else { 1 };
                    pos += 1;
                } else if extra_skip > 0 && pos > 0 && s[pos - 1] == b'-' {
                    out.tzsign = -1;
                } else {
                    out.tzsign = 1;
                }
                parse_int_len(Some(&mut out.tzh), s, &mut pos, 2, nodes, i)?;
            }
            K::TZM => {
                if out.tzsign == 0 {
                    out.tzsign = 1;
                }
                parse_int_len(Some(&mut out.tzm), s, &mut pos, 2, nodes, i)?;
            }
            K::A_D | K::B_C | K::a_d | K::b_c => {
                let v = seq_search(s, &mut pos, &ADBC_STRINGS_LONG, false, k)?;
                set_int(&mut out.bc, v % 2, k)?;
            }
            K::AD | K::BC | K::ad | K::bc => {
                let v = seq_search(s, &mut pos, &ADBC_STRINGS, false, k)?;
                set_int(&mut out.bc, v % 2, k)?;
            }
            K::MONTH | K::Month | K::month => {
                let v = seq_search(s, &mut pos, &MONTHS_FULL, tm_suffix, k)?;
                set_int(&mut out.mm, v + 1, k)?;
            }
            K::MON | K::Mon | K::mon => {
                let v = seq_search(s, &mut pos, &MONTHS, tm_suffix, k)?;
                set_int(&mut out.mm, v + 1, k)?;
            }
            K::MM => {
                parse_int_len(Some(&mut out.mm), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::DAY | K::Day | K::day => {
                let v = seq_search(s, &mut pos, &DAYS, tm_suffix, k)?;
                set_int(&mut out.d, v, k)?;
                out.d += 1;
            }
            K::DY | K::Dy | K::dy => {
                let v = seq_search(s, &mut pos, &DAYS_SHORT, tm_suffix, k)?;
                set_int(&mut out.d, v, k)?;
                out.d += 1;
            }
            K::DDD => {
                parse_int_len(Some(&mut out.ddd), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::IDDD => {
                parse_int_len(Some(&mut out.ddd), s, &mut pos, 3, nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::DD => {
                parse_int_len(Some(&mut out.dd), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::D => {
                parse_int_len(Some(&mut out.d), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::ID => {
                parse_int_len(Some(&mut out.d), s, &mut pos, 1, nodes, i)?;
                out.d = out.d.wrapping_add(1);
                if out.d > 7 {
                    out.d = 1;
                }
                skip_thth(s, &mut pos, n.suffix);
            }
            K::WW | K::IW => {
                parse_int_len(Some(&mut out.ww), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::Q => {
                parse_int_len(None, s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::CC => {
                parse_int_len(Some(&mut out.cc), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::Y_YYY => {
                let rest = cstr(s, pos);
                let Some((millennia, years, nch)) = scan_y_yyy(rest) else {
                    return Err(err(format!(
                        "invalid value \"{}\" for \"Y,YYY\"",
                        text(rest)
                    )));
                };
                let years = i32::try_from(millennia)
                    .ok()
                    .and_then(|m| m.checked_mul(1000))
                    .and_then(|m| (years as i32).checked_add(m))
                    .ok_or_else(|| err("value for \"Y,YYY\" in source string is out of range"))?;
                set_int(&mut out.year, years, k)?;
                out.yysz = 4;
                pos += nch;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::YYYY | K::IYYY => {
                parse_int_len(Some(&mut out.year), s, &mut pos, k.name.len(), nodes, i)?;
                out.yysz = 4;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::YYY | K::IYY | K::YY | K::IY | K::Y | K::I => {
                let len = parse_int_len(Some(&mut out.year), s, &mut pos, k.name.len(), nodes, i)?;
                if len < 4 {
                    out.year = adjust_partial_year_to_2020(out.year);
                }
                out.yysz = match k.id {
                    K::YYY | K::IYY => 3,
                    K::YY | K::IY => 2,
                    _ => 1,
                };
                skip_thth(s, &mut pos, n.suffix);
            }
            K::RM | K::rm => {
                let v = seq_search(s, &mut pos, &RM_MONTHS_LOWER, false, k)?;
                set_int(&mut out.mm, 12 - v, k)?;
            }
            K::W => {
                parse_int_len(Some(&mut out.w), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            K::J => {
                parse_int_len(Some(&mut out.j), s, &mut pos, k.name.len(), nodes, i)?;
                skip_thth(s, &mut pos, n.suffix);
            }
            _ => {}
        }
        if !fx_mode {
            extra_skip = 0;
            while at(s, pos) != 0 && is_space(s[pos]) {
                pos += 1;
                extra_skip += 1;
            }
        }
        i += 1;
    }
    Ok(out)
}

/// A pg_tm as do_to_timestamp fills it.
#[derive(Clone, Copy, Debug)]
struct PgTm {
    year: i32,
    mon: i32,
    mday: i32,
    hour: i32,
    min: i32,
    sec: i32,
}

const DTK_YEAR: u32 = 1 << 2;
const DTK_MONTH: u32 = 1 << 1;
const DTK_DAY: u32 = 1 << 3;
const DTK_DATE_M: u32 = DTK_YEAR | DTK_MONTH | DTK_DAY;

fn field_overflow(s: &str) -> Error {
    err(format!("date/time field value out of range: \"{s}\""))
}

/// do_to_timestamp: a text read as a template asks into a date and time, its fraction of a second,
/// its zone's offset where it names one (seconds west of UTC) and its fraction's precision.
fn do_to_timestamp(input: &str, fmt: &str) -> Result<(PgTm, i32, Option<i32>, i32)> {
    let mut tm = PgTm {
        year: 0,
        mon: 1,
        mday: 1,
        hour: 0,
        min: 0,
        sec: 0,
    };
    let mut fsec: i32 = 0;
    let mut fmask = 0u32;
    let mut tmfc = TmFromChar::default();
    if !fmt.is_empty() {
        let nodes = parse_format(fmt.as_bytes(), true, None)?;
        tmfc = dch_from_char(&nodes, input.as_bytes())?;
    }
    let overflow = || field_overflow(input);
    if tmfc.ssss != 0 {
        let mut x = tmfc.ssss;
        tm.hour = x / 3600;
        x %= 3600;
        tm.min = x / 60;
        x %= 60;
        tm.sec = x;
    }
    if tmfc.ss != 0 {
        tm.sec = tmfc.ss;
    }
    if tmfc.mi != 0 {
        tm.min = tmfc.mi;
    }
    if tmfc.hh != 0 {
        tm.hour = tmfc.hh;
    }
    if tmfc.clock_12_hour {
        if tm.hour < 1 || tm.hour > 12 {
            return Err(err(format!(
                "hour \"{}\" is invalid for the 12-hour clock",
                tm.hour
            )));
        }
        if tmfc.pm != 0 && tm.hour < 12 {
            tm.hour += 12;
        } else if tmfc.pm == 0 && tm.hour == 12 {
            tm.hour = 0;
        }
    }
    if tmfc.year != 0 {
        if tmfc.cc != 0 && tmfc.yysz <= 2 {
            if tmfc.bc != 0 {
                tmfc.cc = -tmfc.cc;
            }
            tm.year = tmfc.year % 100;
            if tm.year != 0 {
                if tmfc.cc >= 0 {
                    tm.year = (tmfc.cc - 1)
                        .checked_mul(100)
                        .and_then(|t| tm.year.checked_add(t))
                        .ok_or_else(overflow)?;
                } else {
                    tm.year = (tmfc.cc + 1)
                        .checked_mul(100)
                        .and_then(|t| t.checked_sub(tm.year))
                        .and_then(|t| t.checked_add(1))
                        .ok_or_else(overflow)?;
                }
            } else {
                tm.year = tmfc
                    .cc
                    .wrapping_mul(100)
                    .wrapping_add(if tmfc.cc >= 0 { 0 } else { 1 });
            }
        } else {
            tm.year = tmfc.year;
            if tmfc.bc != 0 {
                tm.year = tm.year.wrapping_neg();
            }
            if tm.year < 0 {
                tm.year += 1;
            }
        }
        fmask |= DTK_YEAR;
    } else if tmfc.cc != 0 {
        if tmfc.bc != 0 {
            tmfc.cc = -tmfc.cc;
        }
        if tmfc.cc >= 0 {
            tm.year = (tmfc.cc - 1)
                .checked_mul(100)
                .and_then(|t| t.checked_add(1))
                .ok_or_else(overflow)?;
        } else {
            tm.year = tmfc
                .cc
                .checked_mul(100)
                .and_then(|t| t.checked_add(1))
                .ok_or_else(overflow)?;
        }
        fmask |= DTK_YEAR;
    }
    if tmfc.j != 0 {
        let (y, m, d) = j2date(tmfc.j);
        tm.year = y;
        tm.mon = m;
        tm.mday = d;
        fmask |= DTK_DATE_M;
    }
    if tmfc.ww != 0 {
        if tmfc.mode == Some(Mode::IsoWeek) {
            let jday = if tmfc.d != 0 {
                let mut j = isoweek2j(tm.year, tmfc.ww);
                if tmfc.d > 1 {
                    j = j.wrapping_add(tmfc.d - 2);
                } else {
                    j = j.wrapping_add(6);
                }
                j
            } else {
                isoweek2j(tm.year, tmfc.ww)
            };
            let (y, m, d) = j2date(jday);
            tm.year = y;
            tm.mon = m;
            tm.mday = d;
            fmask |= DTK_DATE_M;
        } else {
            tmfc.ddd = tmfc
                .ww
                .checked_sub(1)
                .and_then(|t| t.checked_mul(7))
                .and_then(|t| t.checked_add(1))
                .ok_or_else(overflow)?;
        }
    }
    if tmfc.w != 0 {
        tmfc.dd = tmfc
            .w
            .checked_sub(1)
            .and_then(|t| t.checked_mul(7))
            .and_then(|t| t.checked_add(1))
            .ok_or_else(overflow)?;
    }
    if tmfc.dd != 0 {
        tm.mday = tmfc.dd;
        fmask |= DTK_DAY;
    }
    if tmfc.mm != 0 {
        tm.mon = tmfc.mm;
        fmask |= DTK_MONTH;
    }
    if tmfc.ddd != 0 && (tm.mon <= 1 || tm.mday <= 1) {
        if tm.year == 0 && tmfc.bc == 0 {
            return Err(err("cannot calculate day of year without year information"));
        }
        if tmfc.mode == Some(Mode::IsoWeek) {
            let j0 = isoweek2j(tm.year, 1).wrapping_sub(1);
            let (y, m, d) = j2date(j0.wrapping_add(tmfc.ddd));
            tm.year = y;
            tm.mon = m;
            tm.mday = d;
            fmask |= DTK_DATE_M;
        } else {
            const YSUM: [[i32; 13]; 2] = [
                [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334, 365],
                [0, 31, 60, 91, 121, 152, 182, 213, 244, 274, 305, 335, 366],
            ];
            let y = &YSUM[usize::from(is_leap(tm.year))];
            let mut i = 1;
            while i <= 12 {
                if tmfc.ddd <= y[i] {
                    break;
                }
                i += 1;
            }
            if tm.mon <= 1 {
                tm.mon = i as i32;
            }
            if tm.mday <= 1 {
                tm.mday = tmfc.ddd.wrapping_sub(y[i - 1]);
            }
            fmask |= DTK_MONTH | DTK_DAY;
        }
    }
    if tmfc.ms != 0 {
        fsec = tmfc
            .ms
            .checked_mul(1000)
            .and_then(|t| fsec.checked_add(t))
            .ok_or_else(overflow)?;
    }
    if tmfc.us != 0 {
        fsec = fsec.wrapping_add(tmfc.us);
    }
    if fmask != 0 {
        if fmask & DTK_MONTH != 0 && (tm.mon < 1 || tm.mon > 12) {
            return Err(overflow());
        }
        if fmask & DTK_DAY != 0 && (tm.mday < 1 || tm.mday > 31) {
            return Err(overflow());
        }
        if fmask & DTK_DATE_M == DTK_DATE_M
            && tm.mday > DAY_TAB[usize::from(is_leap(tm.year))][(tm.mon - 1) as usize]
        {
            return Err(overflow());
        }
    }
    if tm.hour < 0
        || tm.hour >= 24
        || tm.min < 0
        || tm.min >= 60
        || tm.sec < 0
        || tm.sec >= 60
        || fsec < 0
        || i64::from(fsec) >= USECS_PER_SEC
    {
        return Err(overflow());
    }
    let mut zone = None;
    if tmfc.tzsign != 0 {
        if tmfc.tzh < 0 || tmfc.tzh > MAX_TZDISP_HOUR || tmfc.tzm < 0 || tmfc.tzm >= 60 {
            return Err(err(format!(
                "time zone displacement out of range: \"{input}\""
            )));
        }
        let off = (tmfc.tzh * 60 + tmfc.tzm) * 60;
        zone = Some(if tmfc.tzsign > 0 { -off } else { off });
    } else if tmfc.has_tz {
        zone = Some(-tmfc.gmtoffset);
    }
    Ok((tm, fsec, zone, tmfc.ff))
}

/// `to_date(text, text)`: the date the text is under the template, in days since 1970-01-01.
pub fn to_date(s: &str, fmt: &str) -> Result<i32> {
    let (tm, _, _, _) = do_to_timestamp(s, fmt)?;
    let out = || err(format!("date out of range: \"{s}\""));
    if !valid_julian(tm.year, tm.mon) {
        return Err(out());
    }
    let result = i64::from(date2j(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    if !(-POSTGRES_EPOCH_JDATE..DATE_END_JULIAN - POSTGRES_EPOCH_JDATE).contains(&result) {
        return Err(out());
    }
    Ok((result + POSTGRES_EPOCH_JDATE - UNIX_EPOCH_JDATE) as i32)
}

/// The text `timestamptz_out` would write for an instant counted from 2000-01-01, under UTC.
fn pg_instant_text(pg: i64) -> String {
    let date = pg.div_euclid(USECS_PER_DAY) + POSTGRES_EPOCH_JDATE;
    let time = pg.rem_euclid(USECS_PER_DAY);
    let (y, m, d) = j2date(date as i32);
    let mut out = format!(
        "{}-{:02}-{:02} {:02}:{:02}:{:02}",
        if y > 0 { y } else { 1 - y },
        m,
        d,
        time / USECS_PER_HOUR,
        time % USECS_PER_HOUR / USECS_PER_MINUTE,
        time % USECS_PER_MINUTE / USECS_PER_SEC
    );
    let frac = time % USECS_PER_SEC;
    if frac != 0 {
        let f = format!("{frac:06}");
        out.push('.');
        out.push_str(f.trim_end_matches('0'));
    }
    out.push_str("+00");
    if y <= 0 {
        out.push_str(" BC");
    }
    out
}

/// `to_timestamp(text, text)`: the instant the text is under the template, in microseconds since
/// 1970-01-01 UTC; the zone the text names, else UTC.
pub fn to_timestamp_text(s: &str, fmt: &str) -> Result<i64> {
    let (tm, fsec, zone, fprec) = do_to_timestamp(s, fmt)?;
    let tz = zone.unwrap_or(0);
    let out = || err("timestamp out of range");
    if !valid_julian(tm.year, tm.mon) {
        return Err(out());
    }
    let date = i64::from(date2j(tm.year, tm.mon, tm.mday)) - POSTGRES_EPOCH_JDATE;
    let time = ((i64::from(tm.hour) * 60 + i64::from(tm.min)) * 60 + i64::from(tm.sec))
        * USECS_PER_SEC
        + i64::from(fsec);
    let mut result = date
        .checked_mul(USECS_PER_DAY)
        .and_then(|r| r.checked_add(time))
        .ok_or_else(out)?;
    result += i64::from(tz) * USECS_PER_SEC;
    if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&result) {
        return Err(out());
    }
    if fprec > 0 && fprec < 6 {
        const SCALES: [i64; 7] = [1_000_000, 100_000, 10_000, 1000, 100, 10, 1];
        const OFFSETS: [i64; 7] = [500_000, 50_000, 5000, 500, 50, 5, 0];
        let p = fprec as usize;
        result = if result >= 0 {
            (result + OFFSETS[p]) / SCALES[p] * SCALES[p]
        } else {
            -(((-result) + OFFSETS[p]) / SCALES[p] * SCALES[p])
        };
        if !(MIN_TIMESTAMP..END_TIMESTAMP).contains(&result) {
            return Err(out());
        }
    }
    result
        .checked_add(EPOCH_SHIFT)
        .filter(|&t| t < i64::MAX)
        .ok_or_else(|| {
            err(format!(
                "the instant {} is past the snail's last instant, 294247-01-10 \
                 04:00:54.775806+00, where Parquet's 64-bit timestamp ends",
                pg_instant_text(result)
            ))
        })
}

// ---------------------------------------------------------------------------------------------
// The numbers

/// NUMProc: the number processor's state, its positions indices into its number and its text.
struct NumProc<'a> {
    is_to_char: bool,
    num: &'a mut NumDesc,
    sign: u8,
    sign_wrote: bool,
    num_count: i32,
    num_in: bool,
    num_curr: i32,
    out_pre_spaces: i32,
    read_dec: bool,
    read_post: i32,
    read_pre: i32,
    number: Vec<u8>,
    number_p: usize,
    inout: Vec<u8>,
    inout_p: usize,
    input_len: usize,
    last_relevant: Option<usize>,
    locale: Locale,
}

/// The symbols a number template reads and writes for its locale's patterns: S's signs, D's
/// decimal point, G's separator and L's currency symbol.
struct Locale {
    negative_sign: Vec<u8>,
    positive_sign: Vec<u8>,
    decimal: Vec<u8>,
    thousands_sep: Vec<u8>,
    currency_symbol: Vec<u8>,
}

/// NUM_prepare_locale: each symbol from the conventions of lc_numeric and lc_monetary, its default
/// where the locale gives none; the decimal point D's alone.
fn num_prepare_locale(num: &NumDesc, conv: &Conv) -> Locale {
    let or = |s: &str, d: &str| if s.is_empty() { d } else { s }.as_bytes().to_vec();
    let decimal = if num.has(NUM_F_LDECIMAL) {
        or(&conv.decimal_point, ".")
    } else {
        b".".to_vec()
    };
    let thousands_sep = if !conv.thousands_sep.is_empty() {
        conv.thousands_sep.as_bytes().to_vec()
    } else if decimal != b"," {
        b",".to_vec()
    } else {
        b".".to_vec()
    };
    Locale {
        negative_sign: or(&conv.negative_sign, "-"),
        positive_sign: or(&conv.positive_sign, "+"),
        decimal,
        thousands_sep,
        currency_symbol: or(&conv.currency_symbol, " "),
    }
}

/// A locale's symbol as NUM_add_locale_symbol writes it: cut to its whole characters within
/// NUM_MAX_ITEM_SIZ bytes.
fn clipped(s: &[u8]) -> &[u8] {
    if s.len() <= NUM_MAX_ITEM_SIZ {
        return s;
    }
    let mut end = 0;
    while end < s.len() {
        let next = end + mblen(s[end]);
        if next > NUM_MAX_ITEM_SIZ {
            break;
        }
        end = next;
    }
    &s[..end]
}

impl NumProc<'_> {
    fn overload(&self) -> bool {
        self.inout_p >= self.input_len
    }

    fn amount(&self, x: usize) -> bool {
        self.inout_p + x <= self.input_len
    }

    fn io(&self) -> u8 {
        at(&self.inout, self.inout_p)
    }

    fn put(&mut self, b: u8) {
        if self.inout_p >= self.inout.len() {
            self.inout.resize(self.inout_p + 64, 0);
        }
        self.inout[self.inout_p] = b;
    }

    fn put_str(&mut self, s: &[u8]) {
        for &b in s {
            self.put(b);
            self.inout_p += 1;
        }
    }

    /// strcpy into the text at its position: the bytes and their NUL, the position kept.
    fn copy_at(&mut self, s: &[u8]) {
        let p = self.inout_p;
        for (i, &b) in s.iter().enumerate() {
            self.inout_p = p + i;
            self.put(b);
        }
        self.inout_p = p + s.len();
        self.put(0);
        self.inout_p = p;
    }

    fn nb(&self, i: usize) -> u8 {
        at(&self.number, i)
    }

    fn is_predec_space(&self) -> bool {
        !self.num.has(NUM_F_ZERO) && self.number_p == 0 && self.nb(0) == b'0' && self.num.post != 0
    }

    fn decimal(&self) -> Vec<u8> {
        self.locale.decimal.clone()
    }

    /// The locale's negative sign, or its positive one, where the text holds it next: its length.
    fn reads_sign(&self, negative: bool) -> Option<usize> {
        let s = if negative {
            &self.locale.negative_sign
        } else {
            &self.locale.positive_sign
        };
        (!s.is_empty() && self.amount(s.len()) && self.inout[self.inout_p..].starts_with(s))
            .then_some(s.len())
    }

    /// S's sign written as NUM_add_locale_symbol writes it: the locale's negative sign or its
    /// positive one.
    fn put_sign(&mut self) {
        let s = if self.sign == b'-' {
            self.locale.negative_sign.clone()
        } else {
            self.locale.positive_sign.clone()
        };
        self.put_str(clipped(&s));
    }

    fn numpart_from_char(&mut self, id: K) {
        let mut isread = false;
        if self.overload() {
            return;
        }
        if self.io() == b' ' {
            self.inout_p += 1;
        }
        if self.overload() {
            return;
        }
        if self.nb(0) == b' ' && (id == K::N0 || id == K::N9) && self.read_pre + self.read_post == 0
        {
            if self.num.has(NUM_F_LSIGN) && self.num.lsign == NUM_LSIGN_PRE {
                if let Some(x) = self.reads_sign(true) {
                    self.inout_p += x;
                    self.number[0] = b'-';
                } else if let Some(x) = self.reads_sign(false) {
                    self.inout_p += x;
                    self.number[0] = b'+';
                }
            } else if self.io() == b'-' || (self.num.has(NUM_F_BRACKET) && self.io() == b'<') {
                self.number[0] = b'-';
                self.inout_p += 1;
            } else if self.io() == b'+' {
                self.number[0] = b'+';
                self.inout_p += 1;
            }
        }
        if self.overload() {
            return;
        }
        if self.io().is_ascii_digit() {
            if self.read_dec && self.read_post == self.num.post {
                return;
            }
            let c = self.io();
            self.number[self.number_p] = c;
            self.number_p += 1;
            if self.read_dec {
                self.read_post += 1;
            } else {
                self.read_pre += 1;
            }
            isread = true;
        } else if self.num.has(NUM_F_DECIMAL) && !self.read_dec {
            let d = self.decimal();
            if self.amount(d.len()) && self.inout[self.inout_p..].starts_with(&d) {
                self.inout_p += d.len() - 1;
                self.number[self.number_p] = b'.';
                self.number_p += 1;
                self.read_dec = true;
                isread = true;
            }
        }
        if self.overload() {
            return;
        }
        if self.nb(0) == b' ' && self.read_pre + self.read_post > 0 {
            if self.num.has(NUM_F_LSIGN)
                && isread
                && self.inout_p + 1 < self.input_len
                && !at(&self.inout, self.inout_p + 1).is_ascii_digit()
            {
                let tmp = self.inout_p;
                self.inout_p += 1;
                if let Some(x) = self.reads_sign(true) {
                    self.inout_p += x - 1;
                    self.number[0] = b'-';
                } else if let Some(x) = self.reads_sign(false) {
                    self.inout_p += x - 1;
                    self.number[0] = b'+';
                }
                if self.nb(0) == b' ' {
                    self.inout_p = tmp;
                }
            } else if !isread
                && !self.num.has(NUM_F_LSIGN)
                && (self.num.has(NUM_F_PLUS) || self.num.has(NUM_F_MINUS))
            {
                let c = self.io();
                if c == b'-' || c == b'+' {
                    self.number[0] = c;
                }
            }
        }
    }

    fn numpart_to_char(&mut self, id: K) {
        if self.num.has(NUM_F_ROMAN) {
            return;
        }
        self.num_in = false;
        let lr_is_dot = self.last_relevant.is_some_and(|l| self.nb(l) == b'.');
        if !self.sign_wrote
            && (self.num_curr >= self.out_pre_spaces
                || (self.num.has(NUM_F_ZERO) && self.num.zero_start == self.num_curr))
            && (!self.is_predec_space() || lr_is_dot)
        {
            if self.num.has(NUM_F_LSIGN) {
                if self.num.lsign == NUM_LSIGN_PRE {
                    self.put_sign();
                    self.sign_wrote = true;
                }
            } else if self.num.has(NUM_F_BRACKET) {
                self.put(if self.sign == b'+' { b' ' } else { b'<' });
                self.inout_p += 1;
                self.sign_wrote = true;
            } else if self.sign == b'+' {
                if !self.num.has(NUM_F_FILLMODE) {
                    self.put(b' ');
                    self.inout_p += 1;
                }
                self.sign_wrote = true;
            } else if self.sign == b'-' {
                self.put(b'-');
                self.inout_p += 1;
                self.sign_wrote = true;
            }
        }
        if matches!(id, K::N9 | K::N0 | K::ND | K::NDec) {
            if self.num_curr < self.out_pre_spaces
                && (self.num.zero_start > self.num_curr || !self.num.has(NUM_F_ZERO))
            {
                if !self.num.has(NUM_F_FILLMODE) {
                    self.put(b' ');
                    self.inout_p += 1;
                }
            } else if self.num.has(NUM_F_ZERO)
                && self.num_curr < self.out_pre_spaces
                && self.num.zero_start <= self.num_curr
            {
                self.put(b'0');
                self.inout_p += 1;
                self.num_in = true;
            } else {
                if self.nb(self.number_p) == b'.' {
                    if !lr_is_dot || self.num.has(NUM_F_FILLMODE) {
                        let d = self.decimal();
                        self.put_str(clipped(&d));
                    }
                } else if self
                    .last_relevant
                    .is_some_and(|l| self.number_p > l && id != K::N0)
                {
                } else if self.is_predec_space() {
                    if !self.num.has(NUM_F_FILLMODE) {
                        self.put(b' ');
                        self.inout_p += 1;
                    } else if lr_is_dot {
                        self.put(b'0');
                        self.inout_p += 1;
                    }
                } else {
                    let c = self.nb(self.number_p);
                    self.put(c);
                    self.inout_p += 1;
                    self.num_in = true;
                }
                if self.nb(self.number_p) != 0 {
                    self.number_p += 1;
                }
            }
            let mut end = self.num_count
                + i32::from(self.out_pre_spaces != 0)
                + i32::from(self.num.has(NUM_F_DECIMAL));
            if self.last_relevant.is_some_and(|l| l == self.number_p) {
                end = self.num_curr;
            }
            if self.num_curr + 1 == end {
                if self.sign_wrote && self.num.has(NUM_F_BRACKET) {
                    self.put(if self.sign == b'+' { b' ' } else { b'>' });
                    self.inout_p += 1;
                } else if self.num.has(NUM_F_LSIGN) && self.num.lsign == NUM_LSIGN_POST {
                    self.put_sign();
                }
            }
        }
        self.num_curr += 1;
    }

    fn eat_non_data_chars(&mut self, n: usize) {
        for _ in 0..n {
            if self.overload() {
                break;
            }
            if b"0123456789.,+-".contains(&self.io()) {
                break;
            }
            self.inout_p += mblen(self.io()).min(self.input_len - self.inout_p);
        }
    }

    fn roman_to_int(&mut self) -> i32 {
        let roman_val = |r: u8| match r {
            b'I' => 1,
            b'V' => 5,
            b'X' => 10,
            b'L' => 50,
            b'C' => 100,
            b'D' => 500,
            b'M' => 1000,
            _ => 0,
        };
        while !self.overload() && is_space(self.io()) {
            self.inout_p += 1;
        }
        let mut chars = Vec::new();
        let mut values = Vec::new();
        while chars.len() < MAX_ROMAN_LEN && !self.overload() {
            let c = self.io().to_ascii_uppercase();
            let v = roman_val(c);
            if v == 0 {
                break;
            }
            chars.push(c);
            values.push(v);
            self.inout_p += 1;
        }
        let len = chars.len();
        if len == 0 {
            return -1;
        }
        let (mut result, mut repeat, mut v_count, mut l_count, mut d_count) = (0, 1, 0, 0, 0);
        let mut subtraction = false;
        let mut last_subtracted = 0;
        let valid_sub = |c: u8, n: u8| {
            (c == b'I' && (n == b'V' || n == b'X'))
                || (c == b'X' && (n == b'L' || n == b'C'))
                || (c == b'C' && (n == b'D' || n == b'M'))
        };
        let mut i = 0;
        while i < len {
            let (c, v) = (chars[i], values[i]);
            if subtraction && v >= last_subtracted {
                return -1;
            }
            if (v_count > 0 && v >= 5) || (l_count > 0 && v >= 50) || (d_count > 0 && v >= 500) {
                return -1;
            }
            match c {
                b'V' => v_count += 1,
                b'L' => l_count += 1,
                b'D' => d_count += 1,
                _ => {}
            }
            if i < len - 1 {
                let (nc, nv) = (chars[i + 1], values[i + 1]);
                if v < nv {
                    if !valid_sub(c, nc) {
                        return -1;
                    }
                    if repeat > 1 {
                        return -1;
                    }
                    if (v_count > 0 && nv >= 5)
                        || (l_count > 0 && nv >= 50)
                        || (d_count > 0 && nv >= 500)
                    {
                        return -1;
                    }
                    match nc {
                        b'V' => v_count += 1,
                        b'L' => l_count += 1,
                        b'D' => d_count += 1,
                        _ => {}
                    }
                    i += 1;
                    repeat = 1;
                    subtraction = true;
                    last_subtracted = v;
                    result += nv - v;
                } else {
                    if c == nc {
                        repeat += 1;
                        if repeat > 3 {
                            return -1;
                        }
                    } else {
                        repeat = 1;
                    }
                    result += v;
                }
            } else {
                result += v;
            }
            i += 1;
        }
        result
    }
}

fn get_last_relevant_decnum(num: &[u8]) -> Option<usize> {
    let s = cstr(num, 0);
    let p = s.iter().position(|&b| b == b'.')?;
    let mut result = p;
    for (i, &b) in s.iter().enumerate().skip(p + 1) {
        if b != b'0' {
            result = i;
        }
    }
    Some(result)
}

/// NUM_processor: a number's text written as a template's nodes ask, or a text read into a number's
/// text.
#[allow(clippy::too_many_arguments)]
fn num_processor(
    nodes: &[Node],
    num: &mut NumDesc,
    inout: Vec<u8>,
    number: Vec<u8>,
    input_len: usize,
    out_pre_spaces: i32,
    sign: u8,
    is_to_char: bool,
    conv: &Conv,
) -> Result<Vec<u8>> {
    if num.zero_start != 0 {
        num.zero_start -= 1;
    }
    if num.has(NUM_F_EEEE) {
        if !is_to_char {
            return Err(err("\"EEEE\" not supported for input"));
        }
        return Ok(cstr(&number, 0).to_vec());
    }
    let locale = num_prepare_locale(num, conv);
    let mut np = NumProc {
        is_to_char,
        num,
        sign: 0,
        sign_wrote: false,
        num_count: 0,
        num_in: false,
        num_curr: 0,
        out_pre_spaces: 0,
        read_dec: false,
        read_post: 0,
        read_pre: 0,
        number,
        number_p: 0,
        inout,
        inout_p: 0,
        input_len,
        last_relevant: None,
        locale,
    };
    if is_to_char {
        np.sign = sign;
        if np.num.has(NUM_F_PLUS) || np.num.has(NUM_F_MINUS) {
            np.sign_wrote = !(np.num.has(NUM_F_PLUS) && !np.num.has(NUM_F_MINUS));
        } else {
            if np.sign != b'-' && np.num.has(NUM_F_FILLMODE) {
                np.num.flag &= !NUM_F_BRACKET;
            }
            np.sign_wrote =
                np.sign == b'+' && np.num.has(NUM_F_FILLMODE) && !np.num.has(NUM_F_LSIGN);
            if np.num.lsign == NUM_LSIGN_PRE && np.num.pre == np.num.pre_lsign_num {
                np.num.lsign = NUM_LSIGN_POST;
            }
        }
    }
    np.num_count = np.num.post + np.num.pre - 1;
    if is_to_char {
        np.out_pre_spaces = out_pre_spaces;
        if np.num.has(NUM_F_FILLMODE) && np.num.has(NUM_F_DECIMAL) {
            np.last_relevant = get_last_relevant_decnum(&np.number);
            if let Some(lr) = np.last_relevant {
                if np.num.zero_end > np.out_pre_spaces {
                    let len = cstr(&np.number, 0).len();
                    let last_zero = (len - 1).min((np.num.zero_end - np.out_pre_spaces) as usize);
                    if lr < last_zero {
                        np.last_relevant = Some(last_zero);
                    }
                }
            }
        }
        if !np.sign_wrote && np.out_pre_spaces == 0 {
            np.num_count += 1;
        }
    } else {
        np.number[0] = b' ';
        np.number[1] = 0;
    }
    np.number_p = if is_to_char { 0 } else { 1 };
    for n in nodes {
        if !np.is_to_char && np.overload() {
            break;
        }
        let Some(k) = n.key() else {
            if np.is_to_char {
                np.put_str(&n.character);
            } else {
                np.inout_p += mblen(np.io())
                    .min(np.input_len.saturating_sub(np.inout_p))
                    .max(1);
            }
            continue;
        };
        match k.id {
            K::N9 | K::N0 | K::NDec | K::ND => {
                if np.is_to_char {
                    np.numpart_to_char(k.id);
                    continue;
                }
                np.numpart_from_char(k.id);
            }
            K::NComma => {
                if np.is_to_char {
                    if !np.num_in {
                        if np.num.has(NUM_F_FILLMODE) {
                            continue;
                        }
                        np.put(b' ');
                    } else {
                        np.put(b',');
                    }
                } else {
                    if !np.num_in && np.num.has(NUM_F_FILLMODE) {
                        continue;
                    }
                    if np.io() != b',' {
                        continue;
                    }
                }
            }
            K::NG => {
                let pattern = np.locale.thousands_sep.clone();
                if np.is_to_char {
                    let shown = clipped(&pattern);
                    if !np.num_in {
                        if np.num.has(NUM_F_FILLMODE) {
                            continue;
                        }
                        for _ in 0..text(shown).chars().count() {
                            np.put(b' ');
                            np.inout_p += 1;
                        }
                        np.inout_p -= 1;
                    } else {
                        np.put_str(shown);
                        np.inout_p -= 1;
                    }
                } else {
                    if !np.num_in && np.num.has(NUM_F_FILLMODE) {
                        continue;
                    }
                    if np.amount(pattern.len()) && np.inout[np.inout_p..].starts_with(&pattern) {
                        np.inout_p += pattern.len() - 1;
                    } else {
                        continue;
                    }
                }
            }
            K::NL => {
                let pattern = np.locale.currency_symbol.clone();
                if np.is_to_char {
                    np.put_str(clipped(&pattern));
                    np.inout_p -= 1;
                } else {
                    np.eat_non_data_chars(text(&pattern).chars().count());
                    continue;
                }
            }
            K::NRN | K::Nrn => {
                if np.is_to_char {
                    let digits = cstr(&np.number, np.number_p).to_vec();
                    let digits = if k.id == K::Nrn {
                        digits.to_ascii_lowercase()
                    } else {
                        digits
                    };
                    let shown = if np.num.has(NUM_F_FILLMODE) {
                        digits
                    } else {
                        format!("{:>15}", text(&digits)).into_bytes()
                    };
                    np.copy_at(&shown);
                    np.inout_p += shown.len().saturating_sub(1);
                } else {
                    let r = np.roman_to_int();
                    if r < 0 {
                        return Err(err("invalid Roman numeral"));
                    }
                    let s = r.to_string().into_bytes();
                    for (i, &b) in s.iter().enumerate() {
                        if np.number_p + i >= np.number.len() {
                            np.number.resize(np.number_p + i + 8, 0);
                        }
                        np.number[np.number_p + i] = b;
                    }
                    np.number_p += s.len();
                    np.num.pre = s.len() as i32;
                    np.num.post = 0;
                    continue;
                }
            }
            K::Nth | K::NTH => {
                if np.num.has(NUM_F_ROMAN)
                    || np.nb(0) == b'#'
                    || np.sign == b'-'
                    || np.num.has(NUM_F_DECIMAL)
                {
                    continue;
                }
                if np.is_to_char {
                    let th = get_th(cstr(&np.number, 0), k.id == K::NTH)?;
                    np.copy_at(th.as_bytes());
                    np.inout_p += 1;
                } else {
                    np.eat_non_data_chars(2);
                    continue;
                }
            }
            K::NMI => {
                if np.is_to_char {
                    if np.sign == b'-' {
                        np.put(b'-');
                    } else if np.num.has(NUM_F_FILLMODE) {
                        continue;
                    } else {
                        np.put(b' ');
                    }
                } else if np.io() == b'-' {
                    np.number[0] = b'-';
                } else {
                    np.eat_non_data_chars(1);
                    continue;
                }
            }
            K::NPL => {
                if np.is_to_char {
                    if np.sign == b'+' {
                        np.put(b'+');
                    } else if np.num.has(NUM_F_FILLMODE) {
                        continue;
                    } else {
                        np.put(b' ');
                    }
                } else if np.io() == b'+' {
                    np.number[0] = b'+';
                } else {
                    np.eat_non_data_chars(1);
                    continue;
                }
            }
            K::NSG => {
                if np.is_to_char {
                    let s = np.sign;
                    np.put(s);
                } else if np.io() == b'-' {
                    np.number[0] = b'-';
                } else if np.io() == b'+' {
                    np.number[0] = b'+';
                } else {
                    np.eat_non_data_chars(1);
                    continue;
                }
            }
            _ => continue,
        }
        np.inout_p += 1;
    }
    if np.is_to_char {
        np.put(0);
        Ok(cstr(&np.inout, 0).to_vec())
    } else {
        if np.number_p > 0 && np.nb(np.number_p - 1) == b'.' {
            np.number[np.number_p - 1] = 0;
        } else {
            if np.number_p >= np.number.len() {
                np.number.resize(np.number_p + 1, 0);
            }
            np.number[np.number_p] = 0;
        }
        np.num.post = np.read_post;
        Ok(cstr(&np.number, 0).to_vec())
    }
}

/// int_to_roman: a number in upper-case Roman numerals, or fifteen #s outside 1 to 3999.
fn int_to_roman(number: i64) -> String {
    if !(1..=3999).contains(&number) {
        return "#".repeat(MAX_ROMAN_LEN);
    }
    let digits = number.to_string();
    let mut out = String::new();
    let mut len = digits.len();
    for c in digits.bytes() {
        let num = i32::from(c) - i32::from(b'1');
        if num >= 0 {
            match len {
                4 => {
                    for _ in 0..=num {
                        out.push('M');
                    }
                }
                3 => out.push_str(RM100[num as usize]),
                2 => out.push_str(RM10[num as usize]),
                _ => out.push_str(RM1[num as usize]),
            }
        }
        len -= 1;
    }
    out
}

/// A number template's text written for a number's text, as NUM_TOCHAR_prepare and _finish write
/// it: an empty template writes nothing.
fn num_to_char(
    nodes: &[Node],
    num: &mut NumDesc,
    fmt_len: usize,
    numstr: &[u8],
    out_pre_spaces: i32,
    sign: u8,
    conv: &Conv,
) -> Result<String> {
    let mut number = numstr.to_vec();
    number.extend_from_slice(&[0; 4]);
    let out = num_processor(
        nodes,
        num,
        vec![0; fmt_len * NUM_MAX_ITEM_SIZ + 1],
        number,
        0,
        out_pre_spaces,
        sign,
        true,
        conv,
    )?;
    Ok(text(&out))
}

/// The digits of an overflowed field: #s, the point where the template puts it.
fn hashes(num: &NumDesc) -> Vec<u8> {
    let mut s = vec![b'#'; (num.pre + num.post + 1).max(0) as usize];
    if let Some(b) = s.get_mut(num.pre.max(0) as usize) {
        *b = b'.';
    }
    s
}

/// A number's text split into its sign and digits, with the spaces before its first digit.
fn sign_and_pre(orgnum: &str, num: &NumDesc) -> (u8, Vec<u8>, i32) {
    let (sign, digits) = match orgnum.strip_prefix('-') {
        Some(d) => (b'-', d),
        None => (b'+', orgnum),
    };
    let pre_len = digits.find('.').unwrap_or(digits.len()) as i32;
    if pre_len < num.pre {
        (sign, digits.as_bytes().to_vec(), num.pre - pre_len)
    } else if pre_len > num.pre {
        (sign, hashes(num), 0)
    } else {
        (sign, digits.as_bytes().to_vec(), 0)
    }
}

/// The most digits PostgreSQL's own snprintf asks of the C library past a double's point; it
/// writes zeros past them.
const PRINTF_PRECISION: usize = 350;

/// `%+.*e` as PostgreSQL's own snprintf writes a finite double: its sign always written, its
/// exponent of at least two digits.
fn c_exp(v: f64, prec: usize) -> String {
    let p = prec.min(PRINTF_PRECISION);
    let s = format!("{v:+.p$e}");
    let (m, e) = s.split_once('e').unwrap();
    let e: i32 = e.parse().unwrap();
    format!(
        "{m}{}e{}{:02}",
        "0".repeat(prec - p),
        if e < 0 { '-' } else { '+' },
        e.abs()
    )
}

/// `%.*f` as PostgreSQL's own snprintf writes it: NaN, Infinity and -Infinity by name.
fn c_fixed(v: f64, prec: usize) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v < 0.0 { "-Infinity" } else { "Infinity" }.into();
    }
    let p = prec.min(PRINTF_PRECISION);
    format!("{v:.p$}{}", "0".repeat(prec - p))
}

fn number_template(fmt: &str) -> Result<Option<(Vec<Node>, NumDesc)>> {
    let len = fmt.len();
    if len == 0 || len >= (i32::MAX as usize - 4) / NUM_MAX_ITEM_SIZ {
        return Ok(None);
    }
    let mut num = NumDesc::default();
    let nodes = parse_format(fmt.as_bytes(), false, Some(&mut num))?;
    Ok(Some((nodes, num)))
}

/// `to_char(bigint, text)`.
pub fn to_char_int8(v: i64, fmt: &str, conv: &Conv) -> Result<String> {
    int_to_char(v, fmt, true, conv)
}

/// `to_char(integer, text)`.
pub fn to_char_int4(v: i32, fmt: &str, conv: &Conv) -> Result<String> {
    int_to_char(i64::from(v), fmt, false, conv)
}

fn int_to_char(value: i64, fmt: &str, wide: bool, conv: &Conv) -> Result<String> {
    let Some((nodes, mut num)) = number_template(fmt)? else {
        return Ok(String::new());
    };
    let mut out_pre_spaces = 0;
    let mut sign = 0u8;
    let numstr: Vec<u8> = if num.has(NUM_F_ROMAN) {
        let v = if wide && i32::try_from(value).is_err() {
            i64::from(i32::MAX)
        } else {
            value
        };
        int_to_roman(v).into_bytes()
    } else if num.has(NUM_F_EEEE) {
        if wide {
            let s = sci(&Numeric::of_int(value), num.post)?;
            if s.starts_with('-') {
                s.into_bytes()
            } else {
                format!(" {s}").into_bytes()
            }
        } else {
            let mut s = c_exp(value as f64, num.post.max(0) as usize).into_bytes();
            if s[0] == b'+' {
                s[0] = b' ';
            }
            s
        }
    } else {
        let mut value = value;
        if num.has(NUM_F_MULTI) {
            let multi = 10f64.powf(f64::from(num.multi));
            if wide {
                let m = float8_to_int(multi, i64::MIN, "bigint")?;
                value = value
                    .checked_mul(m)
                    .ok_or_else(|| err("bigint out of range"))?;
            } else {
                let m = float8_to_int(multi, i64::from(i32::MIN), "integer")?;
                value = i32::try_from(value)
                    .ok()
                    .and_then(|v| v.checked_mul(m as i32))
                    .map(i64::from)
                    .ok_or_else(|| err("integer out of range"))?;
            }
            num.pre += num.multi;
        }
        let orgnum = value.to_string();
        let (s, digits) = match orgnum.strip_prefix('-') {
            Some(d) => (b'-', d.to_string()),
            None => (b'+', orgnum.clone()),
        };
        sign = s;
        let pre_len = digits.len() as i32;
        let mut numstr = digits.into_bytes();
        if num.post > 0 {
            numstr.push(b'.');
            numstr.extend(std::iter::repeat_n(b'0', num.post as usize));
        }
        if pre_len < num.pre {
            out_pre_spaces = num.pre - pre_len;
        } else if pre_len > num.pre {
            numstr = hashes(&num);
        }
        numstr
    };
    num_to_char(
        &nodes,
        &mut num,
        fmt.len(),
        &numstr,
        out_pre_spaces,
        sign,
        conv,
    )
}

/// dtoi4 and dtoi8: a double rounded to an integer, refused below `least` or from its negation
/// up.
fn float8_to_int(f: f64, least: i64, name: &str) -> Result<i64> {
    let r = f.round_ties_even();
    if r.is_nan() || r < least as f64 || r >= -(least as f64) {
        return Err(err(format!("{name} out of range")));
    }
    Ok(r as i64)
}

/// numeric_out_sci: a numeric in scientific notation, its significand rounded to `scale` places.
fn sci(v: &Numeric, scale: i32) -> Result<String> {
    if v.is_nan() {
        return Ok("NaN".into());
    }
    if v.is_infinite() {
        return Ok(if v.sign() < 0 {
            "-Infinity"
        } else {
            "Infinity"
        }
        .into());
    }
    let scale = scale.max(0);
    let t = v.to_string();
    let digits = t.trim_start_matches('-');
    let (int, frac) = digits.split_once('.').unwrap_or((digits, ""));
    let exponent: i32 = if let Some(p) = int.bytes().position(|b| b != b'0') {
        (int.len() - p) as i32 - 1
    } else if let Some(p) = frac.bytes().position(|b| b != b'0') {
        -(p as i32) - 1
    } else {
        0
    };
    let shift = Numeric::parse(&format!("1e{}", -exponent))?;
    let sig = v.mul(&shift)?.round_to(scale)?;
    Ok(format!(
        "{}e{}{:02}",
        sig,
        if exponent < 0 { '-' } else { '+' },
        exponent.abs()
    ))
}

/// `to_char(real, text)`.
pub fn to_char_float4(value: f32, fmt: &str, conv: &Conv) -> Result<String> {
    let Some((nodes, mut num)) = number_template(fmt)? else {
        return Ok(String::new());
    };
    let mut out_pre_spaces = 0;
    let mut sign = 0u8;
    let numstr: Vec<u8> = if num.has(NUM_F_ROMAN) {
        let v = value.round_ties_even();
        let fits = !v.is_nan() && v >= i32::MIN as f32 && v < -(i32::MIN as f32);
        int_to_roman(if fits { v as i64 } else { i64::from(i32::MAX) }).into_bytes()
    } else if num.has(NUM_F_EEEE) {
        if value.is_nan() || value.is_infinite() {
            eeee_hashes(&num)
        } else {
            let mut s = c_exp(f64::from(value), num.post.max(0) as usize).into_bytes();
            if s[0] == b'+' {
                s[0] = b' ';
            }
            s
        }
    } else {
        let mut val = value;
        if num.has(NUM_F_MULTI) {
            let multi = 10f64.powf(f64::from(num.multi)) as f32;
            val = value * multi;
            num.pre += num.multi;
        }
        let pre = c_fixed(f64::from(val).abs(), 0).len() as i32;
        if pre >= FLT_DIG {
            num.post = 0;
        } else if pre + num.post > FLT_DIG {
            num.post = FLT_DIG - pre;
        }
        let orgnum = c_fixed(f64::from(val), num.post.max(0) as usize);
        let (s, digits, spaces) = sign_and_pre(&orgnum, &num);
        sign = s;
        out_pre_spaces = spaces;
        digits
    };
    num_to_char(
        &nodes,
        &mut num,
        fmt.len(),
        &numstr,
        out_pre_spaces,
        sign,
        conv,
    )
}

/// The #s EEEE writes for NaN or an infinity.
fn eeee_hashes(num: &NumDesc) -> Vec<u8> {
    let mut s = vec![b'#'; (num.pre + num.post + 6).max(0) as usize];
    if let Some(b) = s.first_mut() {
        *b = b' ';
    }
    if let Some(b) = s.get_mut((num.pre + 1).max(0) as usize) {
        *b = b'.';
    }
    s
}

/// `to_char(double precision, text)`.
pub fn to_char_float8(value: f64, fmt: &str, conv: &Conv) -> Result<String> {
    let Some((nodes, mut num)) = number_template(fmt)? else {
        return Ok(String::new());
    };
    let mut out_pre_spaces = 0;
    let mut sign = 0u8;
    let numstr: Vec<u8> = if num.has(NUM_F_ROMAN) {
        let v = value.round_ties_even();
        let fits = !v.is_nan() && v >= i32::MIN as f64 && v < -(i32::MIN as f64);
        int_to_roman(if fits { v as i64 } else { i64::from(i32::MAX) }).into_bytes()
    } else if num.has(NUM_F_EEEE) {
        if value.is_nan() || value.is_infinite() {
            eeee_hashes(&num)
        } else {
            let mut s = c_exp(value, num.post.max(0) as usize).into_bytes();
            if s[0] == b'+' {
                s[0] = b' ';
            }
            s
        }
    } else {
        let mut val = value;
        if num.has(NUM_F_MULTI) {
            let multi = 10f64.powf(f64::from(num.multi));
            val = value * multi;
            num.pre += num.multi;
        }
        let pre = c_fixed(val.abs(), 0).len() as i32;
        if pre >= DBL_DIG {
            num.post = 0;
        } else if pre + num.post > DBL_DIG {
            num.post = DBL_DIG - pre;
        }
        let orgnum = c_fixed(val, num.post.max(0) as usize);
        let (s, digits, spaces) = sign_and_pre(&orgnum, &num);
        sign = s;
        out_pre_spaces = spaces;
        digits
    };
    num_to_char(
        &nodes,
        &mut num,
        fmt.len(),
        &numstr,
        out_pre_spaces,
        sign,
        conv,
    )
}

/// `to_char(numeric, text)`.
pub fn to_char_numeric(value: &Numeric, fmt: &str, conv: &Conv) -> Result<String> {
    let Some((nodes, mut num)) = number_template(fmt)? else {
        return Ok(String::new());
    };
    let mut out_pre_spaces = 0;
    let mut sign = 0u8;
    let numstr: Vec<u8> = if num.has(NUM_F_ROMAN) {
        let v = value
            .to_integer(i64::from(i32::MIN), i64::from(i32::MAX), "integer")
            .unwrap_or(i64::from(i32::MAX));
        int_to_roman(v).into_bytes()
    } else if num.has(NUM_F_EEEE) {
        let orgnum = sci(value, num.post)?;
        if matches!(orgnum.as_str(), "NaN" | "Infinity" | "-Infinity") {
            eeee_hashes(&num)
        } else if !orgnum.starts_with('-') {
            format!(" {orgnum}").into_bytes()
        } else {
            orgnum.into_bytes()
        }
    } else {
        let mut val = value.clone();
        if num.has(NUM_F_MULTI) {
            let x = Numeric::of_int(10).power(&Numeric::of_int(i64::from(num.multi)))?;
            val = value.mul(&x)?;
            num.pre += num.multi;
        }
        let orgnum = val.round_to(num.post)?.to_string();
        let (s, digits, spaces) = sign_and_pre(&orgnum, &num);
        sign = s;
        out_pre_spaces = spaces;
        digits
    };
    num_to_char(
        &nodes,
        &mut num,
        fmt.len(),
        &numstr,
        out_pre_spaces,
        sign,
        conv,
    )
}

/// apply_typmod of numeric_in: a numeric held to a precision and scale.
fn held_to_typmod(n: Numeric, precision: i32, scale: i32) -> Result<Numeric> {
    let typmod = ((precision << 16) | scale).wrapping_add(4);
    if typmod < 4 {
        return Ok(n);
    }
    let precision = ((typmod - 4) >> 16) & 0xffff;
    let scale = (((typmod - 4) & 0x7ff) ^ 1024) - 1024;
    n.typmod(precision as u32, scale)
}

/// `to_number(text, text)`: the numeric the text is under the template; none where the template
/// is empty.
pub fn to_number(s: &str, fmt: &str, conv: &Conv) -> Result<Option<Numeric>> {
    let Some((nodes, mut num)) = number_template(fmt)? else {
        return Ok(None);
    };
    let input = s.as_bytes().to_vec();
    let input_len = input.len();
    let number = vec![0u8; fmt.len() * NUM_MAX_ITEM_SIZ + 2];
    let numstr = num_processor(
        &nodes, &mut num, input, number, input_len, 0, 0, false, conv,
    )?;
    let scale = num.post;
    let precision = num.pre + num.multi + scale;
    let text = String::from_utf8_lossy(&numstr).into_owned();
    let mut result = held_to_typmod(Numeric::parse(&text)?, precision, scale)?;
    if num.has(NUM_F_MULTI) {
        let x = Numeric::of_int(10).power(&Numeric::of_int(-i64::from(num.multi)))?;
        result = result.mul(&x)?;
    }
    Ok(Some(result))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> i64 {
        crate::times::timestamp_in(s, None, 0).unwrap()
    }

    #[test]
    fn a_timestamp_is_written_as_its_template_asks() {
        let t = ts("2024-02-29 13:04:05.678901");
        assert_eq!(
            to_char_timestamp(t, "YYYY-MM-DD HH24:MI:SS.US Day DY Month MON").unwrap(),
            Some("2024-02-29 13:04:05.678901 Thursday  THU February  FEB".to_string())
        );
        assert_eq!(
            to_char_timestamp(
                t,
                "FMDay, FMDDth \"of\" FMMonth IYYY-IW-ID HH12 a.m. Q CC J"
            )
            .unwrap()
            .unwrap(),
            "Thursday, 29th of February 2024-09-4 01 p.m. 1 21 2460370"
        );
        assert_eq!(to_char_timestamp(t, "").unwrap(), None);
        assert_eq!(
            to_char_timestamptz(t, "TZ tz OF TZH:TZM").unwrap().unwrap(),
            "UTC utc +00 +00:00"
        );
    }

    fn c() -> &'static Conv {
        crate::monetary::Monetary::C.conv()
    }

    #[test]
    fn a_number_is_written_as_its_template_asks() {
        assert_eq!(to_char_int4(-1234, "9,999.99", c()).unwrap(), "-1,234.00");
        assert_eq!(to_char_int4(5, "FM999th", c()).unwrap(), "5th");
        assert_eq!(to_char_int4(1994, "RN", c()).unwrap(), "        MCMXCIV");
        assert_eq!(to_char_float8(-0.1, "S9.99", c()).unwrap(), " -.10");
        assert_eq!(
            to_char_float8(123.456, "9.99EEEE", c()).unwrap(),
            " 1.23e+02"
        );
        assert_eq!(to_char_int4(12345, "99", c()).unwrap(), " ##");
    }

    #[test]
    fn l_writes_lc_monetary_s_currency_symbol_and_d_and_g_lc_numeric_s() {
        let n = |s: &str| Numeric::parse(s).unwrap();
        let at = |symbol: &str| {
            Conv::of(&[
                ("currency_symbol", symbol),
                ("mon_decimal_point", ","),
                ("mon_thousands_sep", "."),
                ("negative_sign", "-"),
            ])
        };
        let (dk, gb) = (at("kr."), at("£"));
        let written = |v: &str, fmt: &str, conv: &Conv| to_char_numeric(&n(v), fmt, conv).unwrap();
        assert_eq!(written("1234.5", "L9G999D99", &dk), "kr. 1,234.50");
        assert_eq!(written("-1234.5", "L9G999D99S", &gb), "£1,234.50-");
        assert_eq!(written("1234.5", "FML9,999.99", &gb), "£1,234.5");
        assert_eq!(written("-1234.5", "L9G999D99S", c()), " 1,234.50-");
        let read =
            |s: &str, conv: &Conv| to_number(s, "L9G999D9", conv).unwrap().unwrap().to_string();
        assert_eq!(read("kr. 1,234.5", &dk), "0.1");
        assert_eq!(read("£1,234.5", &gb), "1234.5");
    }

    #[test]
    fn a_text_is_read_as_its_template_asks() {
        assert_eq!(
            to_number("-12,454.8", "99G999D9S", c())
                .unwrap()
                .unwrap()
                .to_string(),
            "-12454.8"
        );
        let d = to_date("05 Dec 2000", "DD Mon YYYY").unwrap();
        assert_eq!(crate::times::date_out(d), "2000-12-05");
        let t = to_timestamp_text("2011-12-18 11:38 PM", "YYYY-MM-DD HH:MI PM").unwrap();
        assert_eq!(crate::times::timestamptz_out(t), "2011-12-18 23:38:00+00");
        assert_eq!(
            to_date("2000 13", "YYYY MM").unwrap_err().to_string(),
            "date/time field value out of range: \"2000 13\""
        );
    }
}
