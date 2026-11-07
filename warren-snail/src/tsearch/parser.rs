//! PostgreSQL 19's default text search parser (`wparser_def.c`): the tokens `ts_parse('default',
//! text)` gives, each with its token type, in a database whose encoding is UTF8 and whose ctype is
//! glibc's C.UTF-8 under the libc provider. Characters are classified as PostgreSQL classifies
//! them there, by glibc's `iswalpha_l` and friends under that locale.



/// The rows of `ts_token_type('default')`: each token type's id, alias and description
/// (`prsd_lextype`).
pub(crate) const TOKEN_TYPES: [(i32, &str, &str); 23] = [
    (1, "asciiword", "Word, all ASCII"),
    (2, "word", "Word, all letters"),
    (3, "numword", "Word, letters and digits"),
    (4, "email", "Email address"),
    (5, "url", "URL"),
    (6, "host", "Host"),
    (7, "sfloat", "Scientific notation"),
    (8, "version", "Version number"),
    (
        9,
        "hword_numpart",
        "Hyphenated word part, letters and digits",
    ),
    (10, "hword_part", "Hyphenated word part, all letters"),
    (11, "hword_asciipart", "Hyphenated word part, all ASCII"),
    (12, "blank", "Space symbols"),
    (13, "tag", "XML tag"),
    (14, "protocol", "Protocol head"),
    (15, "numhword", "Hyphenated word, letters and digits"),
    (16, "asciihword", "Hyphenated word, all ASCII"),
    (17, "hword", "Hyphenated word, all letters"),
    (18, "url_path", "URL path"),
    (19, "file", "File or path name"),
    (20, "float", "Decimal notation"),
    (21, "int", "Signed integer"),
    (22, "uint", "Unsigned integer"),
    (23, "entity", "XML entity"),
];

const ASCIIWORD: i32 = 1;
const WORD_T: i32 = 2;
const NUMWORD: i32 = 3;
const EMAIL: i32 = 4;
const URL_T: i32 = 5;
const HOST: i32 = 6;
const SCIENTIFIC: i32 = 7;
const VERSIONNUMBER: i32 = 8;
const NUMPARTHWORD: i32 = 9;
const PARTHWORD: i32 = 10;
const ASCIIPARTHWORD: i32 = 11;
const SPACE: i32 = 12;
const TAG_T: i32 = 13;
const PROTOCOL: i32 = 14;
const NUMHWORD: i32 = 15;
const ASCIIHWORD: i32 = 16;
const HWORD: i32 = 17;
const URLPATH: i32 = 18;
const FILEPATH: i32 = 19;
const DECIMAL_T: i32 = 20;
const SIGNEDINT: i32 = 21;
const UNSIGNEDINT: i32 = 22;
const XMLENTITY: i32 = 23;

/// The tokens of `text` as `ts_parse('default', text)` gives them, in order: each token type's id
/// and the token's text (`prsd_start`, then `prsd_nexttoken` until it gives none).
pub(crate) fn parse(text: &str) -> Vec<(i32, String)> {
    let s = text.as_bytes();
    let w = to_wide(s);
    c_utf8();
    let mut p = Parser::new(s, &w);
    let mut out = Vec::new();
    while p.get() {
        let tok = &s[p.token..p.token + p.lenbytetoken];
        out.push((p.ty, String::from_utf8_lossy(tok).into_owned()));
    }
    out
}

/// glibc's C.UTF-8 locale for LC_CTYPE, which the default text search parser classifies characters
/// under.
fn c_utf8() {
    if let Err(e) = crate::sys::ctype::found("the default text search parser classifies characters under") {
        panic!("{}", e.0);
    }
}

/// Whether glibc's C.UTF-8 takes a character as a letter or a digit: `t_isalnum_cstr`
/// (ts_locale.c), as `parse_or_operator` asks it.
pub(crate) fn is_alnum(c: char) -> bool {
    c_utf8();
    crate::sys::ctype::is(crate::sys::ctype::Class::Alnum, c as u32)
}

/// The text's characters as code points, then a zero, one slot per byte and one more
/// (`pg_mb2wchar_with_len` under UTF8, as `TParserInit` allocates it). It stops at a NUL byte,
/// which PostgreSQL's text never holds.
fn to_wide(s: &[u8]) -> Vec<u32> {
    let mut w = vec![0u32; s.len() + 1];
    let mut i = 0;
    let mut n = 0;
    while i < s.len() && s[i] != 0 {
        let b = s[i];
        let (c, l) = if b & 0x80 == 0 {
            (b as u32, 1)
        } else if b & 0xe0 == 0xc0 && i + 2 <= s.len() {
            ((((b & 0x1f) as u32) << 6) | (s[i + 1] & 0x3f) as u32, 2)
        } else if b & 0xf0 == 0xe0 && i + 3 <= s.len() {
            (
                (((b & 0x0f) as u32) << 12)
                    | (((s[i + 1] & 0x3f) as u32) << 6)
                    | (s[i + 2] & 0x3f) as u32,
                3,
            )
        } else if b & 0xf8 == 0xf0 && i + 4 <= s.len() {
            (
                (((b & 0x07) as u32) << 18)
                    | (((s[i + 1] & 0x3f) as u32) << 12)
                    | (((s[i + 2] & 0x3f) as u32) << 6)
                    | (s[i + 3] & 0x3f) as u32,
                4,
            )
        } else if b & 0xe0 == 0xc0 || b & 0xf0 == 0xe0 || b & 0xf8 == 0xf0 {
            break;
        } else {
            (b as u32, 1)
        };
        w[n] = c;
        n += 1;
        i += l;
    }
    w
}

/// The byte length of the character a UTF-8 sequence begins with (`pg_utf_mblen`).
fn utf_mblen(b: u8) -> usize {
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

/// The code point a UTF-8 sequence begins with (`utf8_to_unicode`); a byte past the end reads
/// as zero.
fn utf8_to_unicode(s: &[u8]) -> u32 {
    let at = |i: usize| s.get(i).copied().unwrap_or(0) as u32;
    let c0 = at(0);
    if c0 & 0x80 == 0 {
        c0
    } else if c0 & 0xe0 == 0xc0 {
        ((c0 & 0x1f) << 6) | (at(1) & 0x3f)
    } else if c0 & 0xf0 == 0xe0 {
        ((c0 & 0x0f) << 12) | ((at(1) & 0x3f) << 6) | (at(2) & 0x3f)
    } else if c0 & 0xf8 == 0xf0 {
        ((c0 & 0x07) << 18) | ((at(1) & 0x3f) << 12) | ((at(2) & 0x3f) << 6) | (at(3) & 0x3f)
    } else {
        0xffff_ffff
    }
}

/// Whether a character's display width is zero (`ucs_wcwidth` returning 0, as `pg_dsplen` under
/// UTF8 gives it): the NUL character and the non-spacing characters.
fn zero_width(ucs: u32) -> bool {
    if ucs == 0 {
        return true;
    }
    if ucs < 0x20 || (0x7f..0xa0).contains(&ucs) || ucs > 0x0010_ffff {
        return false;
    }
    NONSPACING
        .binary_search_by(|&(first, last)| {
            if last < ucs {
                std::cmp::Ordering::Less
            } else if first > ucs {
                std::cmp::Ordering::Greater
            } else {
                std::cmp::Ordering::Equal
            }
        })
        .is_ok()
}

/// The parser's states (`TParserState`), in its order; `Null` leaves the state unchanged.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
enum St {
    Base = 0,
    InNumWord,
    InAsciiWord,
    InWord,
    InUnsignedInt,
    InSignedIntFirst,
    InSignedInt,
    InSpace,
    InUDecimalFirst,
    InUDecimal,
    InDecimalFirst,
    InDecimal,
    InVerVersion,
    InSVerVersion,
    InVersionFirst,
    InVersion,
    InMantissaFirst,
    InMantissaSign,
    InMantissa,
    InXMLEntityFirst,
    InXMLEntity,
    InXMLEntityNumFirst,
    InXMLEntityNum,
    InXMLEntityHexNumFirst,
    InXMLEntityHexNum,
    InXMLEntityEnd,
    InTagFirst,
    InXMLBegin,
    InTagCloseFirst,
    InTagName,
    InTagBeginEnd,
    InTag,
    InTagEscapeK,
    InTagEscapeKK,
    InTagBackSleshed,
    InTagEnd,
    InCommentFirst,
    InCommentLast,
    InComment,
    InCloseCommentFirst,
    InCloseCommentLast,
    InCommentEnd,
    InHostFirstDomain,
    InHostDomainSecond,
    InHostDomain,
    InPortFirst,
    InPort,
    InHostFirstAN,
    InHost,
    InEmail,
    InFileFirst,
    InFileTwiddle,
    InPathFirst,
    InPathFirstFirst,
    InPathSecond,
    InFile,
    InFileNext,
    InURLPathFirst,
    InURLPathStart,
    InURLPath,
    InFURL,
    InProtocolFirst,
    InProtocolSecond,
    InProtocolEnd,
    InHyphenAsciiWordFirst,
    InHyphenAsciiWord,
    InHyphenWordFirst,
    InHyphenWord,
    InHyphenNumWordFirst,
    InHyphenNumWord,
    InHyphenDigitLookahead,
    InParseHyphen,
    InParseHyphenHyphen,
    InHyphenWordPart,
    InHyphenAsciiWordPart,
    InHyphenNumWordPart,
    InHyphenUnsignedInt,
    Null,
}

/// A row's character test: the `p_is*` functions, `p_iseqC` with its character, and `Any` for a
/// row without a test, which always matches.
#[derive(Clone, Copy, Debug)]
enum T {
    Eof,
    Eq(u8),
    Alnum,
    NotAlnum,
    Alpha,
    Digit,
    Space,
    XDigit,
    AscLet,
    Special,
    Ignore,
    StopHost,
    Host,
    UrlPath,
    UrlChar,
    Any,
}

/// A row's special handler (`SpecialTags`, `SpecialFURL`, `SpecialHyphen`, `SpecialVerVersion`).
#[derive(Clone, Copy, Debug)]
enum Sp {
    No,
    Tags,
    FUrl,
    Hyphen,
    VerVersion,
}

const NEXT: u8 = 0x00;
const BINGO: u8 = 0x01;
const POP: u8 = 0x02;
const PUSH: u8 = 0x04;
const RERUN: u8 = 0x08;
const CLEAR: u8 = 0x10;
const MERGE: u8 = 0x20;
const CLRALL: u8 = 0x40;

/// One row of a state's table (`TParserStateActionItem`).
#[derive(Clone, Copy, Debug)]
struct Item {
    test: T,
    flags: u8,
    to: St,
    ty: i32,
    special: Sp,
}

/// A row with no special handler.
const fn r(test: T, flags: u8, to: St, ty: i32) -> Item {
    Item {
        test,
        flags,
        to,
        ty,
        special: Sp::No,
    }
}

/// A row with a special handler.
const fn rs(test: T, flags: u8, to: St, ty: i32, special: Sp) -> Item {
    Item {
        test,
        flags,
        to,
        ty,
        special,
    }
}

use self::St::*;
use self::T::*;

/// Each state's table, in the states' order (`Actions`).
static ACTIONS: [(St, &[Item]); 78] = [
    (
        Base,
        &[
            r(Eof, NEXT, Null, 0),
            r(Eq(b'<'), PUSH, InTagFirst, 0),
            r(Ignore, NEXT, InSpace, 0),
            r(AscLet, NEXT, InAsciiWord, 0),
            r(Alpha, NEXT, InWord, 0),
            r(Digit, NEXT, InUnsignedInt, 0),
            r(Eq(b'-'), PUSH, InSignedIntFirst, 0),
            r(Eq(b'+'), PUSH, InSignedIntFirst, 0),
            r(Eq(b'&'), PUSH, InXMLEntityFirst, 0),
            r(Eq(b'~'), PUSH, InFileTwiddle, 0),
            r(Eq(b'/'), PUSH, InFileFirst, 0),
            r(Eq(b'.'), PUSH, InPathFirstFirst, 0),
            r(Any, NEXT, InSpace, 0),
        ],
    ),
    (
        InNumWord,
        &[
            r(Eof, BINGO, Base, NUMWORD),
            r(Alnum, NEXT, InNumWord, 0),
            r(Special, NEXT, InNumWord, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(Eq(b'/'), PUSH, InFileFirst, 0),
            r(Eq(b'.'), PUSH, InFileNext, 0),
            r(Eq(b'-'), PUSH, InHyphenNumWordFirst, 0),
            r(Any, BINGO, Base, NUMWORD),
        ],
    ),
    (
        InAsciiWord,
        &[
            r(Eof, BINGO, Base, ASCIIWORD),
            r(AscLet, NEXT, Null, 0),
            r(Eq(b'.'), PUSH, InHostFirstDomain, 0),
            r(Eq(b'.'), PUSH, InFileNext, 0),
            r(Eq(b'-'), PUSH, InHostFirstAN, 0),
            r(Eq(b'-'), PUSH, InHyphenAsciiWordFirst, 0),
            r(Eq(b'_'), PUSH, InHostFirstAN, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(Eq(b':'), PUSH, InProtocolFirst, 0),
            r(Eq(b'/'), PUSH, InFileFirst, 0),
            r(Digit, PUSH, InHost, 0),
            r(Digit, NEXT, InNumWord, 0),
            r(Alpha, NEXT, InWord, 0),
            r(Special, NEXT, InWord, 0),
            r(Any, BINGO, Base, ASCIIWORD),
        ],
    ),
    (
        InWord,
        &[
            r(Eof, BINGO, Base, WORD_T),
            r(Alpha, NEXT, Null, 0),
            r(Special, NEXT, Null, 0),
            r(Digit, NEXT, InNumWord, 0),
            r(Eq(b'-'), PUSH, InHyphenWordFirst, 0),
            r(Any, BINGO, Base, WORD_T),
        ],
    ),
    (
        InUnsignedInt,
        &[
            r(Eof, BINGO, Base, UNSIGNEDINT),
            r(Digit, NEXT, Null, 0),
            r(Eq(b'.'), PUSH, InHostFirstDomain, 0),
            r(Eq(b'.'), PUSH, InUDecimalFirst, 0),
            r(Eq(b'e'), PUSH, InMantissaFirst, 0),
            r(Eq(b'E'), PUSH, InMantissaFirst, 0),
            r(Eq(b'-'), PUSH, InHostFirstAN, 0),
            r(Eq(b'_'), PUSH, InHostFirstAN, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(AscLet, PUSH, InHost, 0),
            r(Alpha, NEXT, InNumWord, 0),
            r(Special, NEXT, InNumWord, 0),
            r(Eq(b'/'), PUSH, InFileFirst, 0),
            r(Any, BINGO, Base, UNSIGNEDINT),
        ],
    ),
    (
        InSignedIntFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT | CLEAR, InSignedInt, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InSignedInt,
        &[
            r(Eof, BINGO, Base, SIGNEDINT),
            r(Digit, NEXT, Null, 0),
            r(Eq(b'.'), PUSH, InDecimalFirst, 0),
            r(Eq(b'e'), PUSH, InMantissaFirst, 0),
            r(Eq(b'E'), PUSH, InMantissaFirst, 0),
            r(Any, BINGO, Base, SIGNEDINT),
        ],
    ),
    (
        InSpace,
        &[
            r(Eof, BINGO, Base, SPACE),
            r(Eq(b'<'), BINGO, Base, SPACE),
            r(Ignore, NEXT, Null, 0),
            r(Eq(b'-'), BINGO, Base, SPACE),
            r(Eq(b'+'), BINGO, Base, SPACE),
            r(Eq(b'&'), BINGO, Base, SPACE),
            r(Eq(b'/'), BINGO, Base, SPACE),
            r(NotAlnum, NEXT, InSpace, 0),
            r(Any, BINGO, Base, SPACE),
        ],
    ),
    (
        InUDecimalFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, CLEAR, InUDecimal, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InUDecimal,
        &[
            r(Eof, BINGO, Base, DECIMAL_T),
            r(Digit, NEXT, InUDecimal, 0),
            r(Eq(b'.'), PUSH, InVersionFirst, 0),
            r(Eq(b'e'), PUSH, InMantissaFirst, 0),
            r(Eq(b'E'), PUSH, InMantissaFirst, 0),
            r(Any, BINGO, Base, DECIMAL_T),
        ],
    ),
    (
        InDecimalFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, CLEAR, InDecimal, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InDecimal,
        &[
            r(Eof, BINGO, Base, DECIMAL_T),
            r(Digit, NEXT, InDecimal, 0),
            r(Eq(b'.'), PUSH, InVerVersion, 0),
            r(Eq(b'e'), PUSH, InMantissaFirst, 0),
            r(Eq(b'E'), PUSH, InMantissaFirst, 0),
            r(Any, BINGO, Base, DECIMAL_T),
        ],
    ),
    (
        InVerVersion,
        &[
            r(Eof, POP, Null, 0),
            rs(Digit, RERUN, InSVerVersion, 0, Sp::VerVersion),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InSVerVersion,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, BINGO | CLRALL, InUnsignedInt, SPACE),
            r(Any, NEXT, Null, 0),
        ],
    ),
    (
        InVersionFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, CLEAR, InVersion, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InVersion,
        &[
            r(Eof, BINGO, Base, VERSIONNUMBER),
            r(Digit, NEXT, InVersion, 0),
            r(Eq(b'.'), PUSH, InVersionFirst, 0),
            r(Any, BINGO, Base, VERSIONNUMBER),
        ],
    ),
    (
        InMantissaFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, CLEAR, InMantissa, 0),
            r(Eq(b'+'), NEXT, InMantissaSign, 0),
            r(Eq(b'-'), NEXT, InMantissaSign, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InMantissaSign,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, CLEAR, InMantissa, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InMantissa,
        &[
            r(Eof, BINGO, Base, SCIENTIFIC),
            r(Digit, NEXT, InMantissa, 0),
            r(Any, BINGO, Base, SCIENTIFIC),
        ],
    ),
    (
        InXMLEntityFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'#'), NEXT, InXMLEntityNumFirst, 0),
            r(AscLet, NEXT, InXMLEntity, 0),
            r(Eq(b':'), NEXT, InXMLEntity, 0),
            r(Eq(b'_'), NEXT, InXMLEntity, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLEntity,
        &[
            r(Eof, POP, Null, 0),
            r(Alnum, NEXT, InXMLEntity, 0),
            r(Eq(b':'), NEXT, InXMLEntity, 0),
            r(Eq(b'_'), NEXT, InXMLEntity, 0),
            r(Eq(b'.'), NEXT, InXMLEntity, 0),
            r(Eq(b'-'), NEXT, InXMLEntity, 0),
            r(Eq(b';'), NEXT, InXMLEntityEnd, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLEntityNumFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'x'), NEXT, InXMLEntityHexNumFirst, 0),
            r(Eq(b'X'), NEXT, InXMLEntityHexNumFirst, 0),
            r(Digit, NEXT, InXMLEntityNum, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLEntityNum,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, InXMLEntityNum, 0),
            r(Eq(b';'), NEXT, InXMLEntityEnd, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLEntityHexNumFirst,
        &[
            r(Eof, POP, Null, 0),
            r(XDigit, NEXT, InXMLEntityHexNum, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLEntityHexNum,
        &[
            r(Eof, POP, Null, 0),
            r(XDigit, NEXT, InXMLEntityHexNum, 0),
            r(Eq(b';'), NEXT, InXMLEntityEnd, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (InXMLEntityEnd, &[r(Any, BINGO | CLEAR, Base, XMLENTITY)]),
    (
        InTagFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'/'), PUSH, InTagCloseFirst, 0),
            r(Eq(b'!'), PUSH, InCommentFirst, 0),
            r(Eq(b'?'), PUSH, InXMLBegin, 0),
            r(AscLet, PUSH, InTagName, 0),
            r(Eq(b':'), PUSH, InTagName, 0),
            r(Eq(b'_'), PUSH, InTagName, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InXMLBegin,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'x'), NEXT, InTag, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InTagCloseFirst,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InTagName, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InTagName,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'/'), NEXT, InTagBeginEnd, 0),
            rs(Eq(b'>'), NEXT, InTagEnd, 0, Sp::Tags),
            rs(Space, NEXT, InTag, 0, Sp::Tags),
            r(Alnum, NEXT, Null, 0),
            r(Eq(b':'), NEXT, Null, 0),
            r(Eq(b'_'), NEXT, Null, 0),
            r(Eq(b'.'), NEXT, Null, 0),
            r(Eq(b'-'), NEXT, Null, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InTagBeginEnd,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'>'), NEXT, InTagEnd, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InTag,
        &[
            r(Eof, POP, Null, 0),
            rs(Eq(b'>'), NEXT, InTagEnd, 0, Sp::Tags),
            r(Eq(b'\''), NEXT, InTagEscapeK, 0),
            r(Eq(b'"'), NEXT, InTagEscapeKK, 0),
            r(AscLet, NEXT, Null, 0),
            r(Digit, NEXT, Null, 0),
            r(Eq(b'='), NEXT, Null, 0),
            r(Eq(b'-'), NEXT, Null, 0),
            r(Eq(b'_'), NEXT, Null, 0),
            r(Eq(b'#'), NEXT, Null, 0),
            r(Eq(b'/'), NEXT, Null, 0),
            r(Eq(b':'), NEXT, Null, 0),
            r(Eq(b'.'), NEXT, Null, 0),
            r(Eq(b'&'), NEXT, Null, 0),
            r(Eq(b'?'), NEXT, Null, 0),
            r(Eq(b'%'), NEXT, Null, 0),
            r(Eq(b'~'), NEXT, Null, 0),
            rs(Space, NEXT, Null, 0, Sp::Tags),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InTagEscapeK,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'\\'), PUSH, InTagBackSleshed, 0),
            r(Eq(b'\''), NEXT, InTag, 0),
            r(Any, NEXT, InTagEscapeK, 0),
        ],
    ),
    (
        InTagEscapeKK,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'\\'), PUSH, InTagBackSleshed, 0),
            r(Eq(b'"'), NEXT, InTag, 0),
            r(Any, NEXT, InTagEscapeKK, 0),
        ],
    ),
    (
        InTagBackSleshed,
        &[r(Eof, POP, Null, 0), r(Any, MERGE, Null, 0)],
    ),
    (InTagEnd, &[r(Any, BINGO | CLRALL, Base, TAG_T)]),
    (
        InCommentFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'-'), NEXT, InCommentLast, 0),
            r(Eq(b'D'), NEXT, InTag, 0),
            r(Eq(b'd'), NEXT, InTag, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InCommentLast,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'-'), NEXT, InComment, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InComment,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'-'), NEXT, InCloseCommentFirst, 0),
            r(Any, NEXT, Null, 0),
        ],
    ),
    (
        InCloseCommentFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'-'), NEXT, InCloseCommentLast, 0),
            r(Any, NEXT, InComment, 0),
        ],
    ),
    (
        InCloseCommentLast,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'-'), NEXT, Null, 0),
            r(Eq(b'>'), NEXT, InCommentEnd, 0),
            r(Any, NEXT, InComment, 0),
        ],
    ),
    (InCommentEnd, &[r(Any, BINGO | CLRALL, Base, TAG_T)]),
    (
        InHostFirstDomain,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InHostDomainSecond, 0),
            r(Digit, NEXT, InHost, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHostDomainSecond,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InHostDomain, 0),
            r(Digit, PUSH, InHost, 0),
            r(Eq(b'-'), PUSH, InHostFirstAN, 0),
            r(Eq(b'_'), PUSH, InHostFirstAN, 0),
            r(Eq(b'.'), PUSH, InHostFirstDomain, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHostDomain,
        &[
            r(Eof, BINGO | CLRALL, Base, HOST),
            r(AscLet, NEXT, InHostDomain, 0),
            r(Digit, PUSH, InHost, 0),
            r(Eq(b':'), PUSH, InPortFirst, 0),
            r(Eq(b'-'), PUSH, InHostFirstAN, 0),
            r(Eq(b'_'), PUSH, InHostFirstAN, 0),
            r(Eq(b'.'), PUSH, InHostFirstDomain, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(Digit, POP, Null, 0),
            r(StopHost, BINGO | CLRALL, InURLPathStart, HOST),
            r(Eq(b'/'), PUSH, InFURL, 0),
            r(Any, BINGO | CLRALL, Base, HOST),
        ],
    ),
    (
        InPortFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, InPort, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InPort,
        &[
            r(Eof, BINGO | CLRALL, Base, HOST),
            r(Digit, NEXT, InPort, 0),
            r(StopHost, BINGO | CLRALL, InURLPathStart, HOST),
            r(Eq(b'/'), PUSH, InFURL, 0),
            r(Any, BINGO | CLRALL, Base, HOST),
        ],
    ),
    (
        InHostFirstAN,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, InHost, 0),
            r(AscLet, NEXT, InHost, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHost,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, InHost, 0),
            r(AscLet, NEXT, InHost, 0),
            r(Eq(b'@'), PUSH, InEmail, 0),
            r(Eq(b'.'), PUSH, InHostFirstDomain, 0),
            r(Eq(b'-'), PUSH, InHostFirstAN, 0),
            r(Eq(b'_'), PUSH, InHostFirstAN, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InEmail,
        &[
            r(StopHost, POP, Null, 0),
            r(Host, BINGO | CLRALL, Base, EMAIL),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InFileFirst,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InFile, 0),
            r(Digit, NEXT, InFile, 0),
            r(Eq(b'.'), NEXT, InPathFirst, 0),
            r(Eq(b'_'), NEXT, InFile, 0),
            r(Eq(b'~'), PUSH, InFileTwiddle, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InFileTwiddle,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InFile, 0),
            r(Digit, NEXT, InFile, 0),
            r(Eq(b'_'), NEXT, InFile, 0),
            r(Eq(b'/'), NEXT, InFileFirst, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InPathFirst,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InFile, 0),
            r(Digit, NEXT, InFile, 0),
            r(Eq(b'_'), NEXT, InFile, 0),
            r(Eq(b'.'), NEXT, InPathSecond, 0),
            r(Eq(b'/'), NEXT, InFileFirst, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InPathFirstFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'.'), NEXT, InPathSecond, 0),
            r(Eq(b'/'), NEXT, InFileFirst, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InPathSecond,
        &[
            r(Eof, BINGO | CLEAR, Base, FILEPATH),
            r(Eq(b'/'), NEXT | PUSH, InFileFirst, 0),
            r(Eq(b'/'), BINGO | CLEAR, Base, FILEPATH),
            r(Space, BINGO | CLEAR, Base, FILEPATH),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InFile,
        &[
            r(Eof, BINGO, Base, FILEPATH),
            r(AscLet, NEXT, InFile, 0),
            r(Digit, NEXT, InFile, 0),
            r(Eq(b'.'), PUSH, InFileNext, 0),
            r(Eq(b'_'), NEXT, InFile, 0),
            r(Eq(b'-'), NEXT, InFile, 0),
            r(Eq(b'/'), PUSH, InFileFirst, 0),
            r(Any, BINGO, Base, FILEPATH),
        ],
    ),
    (
        InFileNext,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, CLEAR, InFile, 0),
            r(Digit, CLEAR, InFile, 0),
            r(Eq(b'_'), CLEAR, InFile, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InURLPathFirst,
        &[
            r(Eof, POP, Null, 0),
            r(UrlChar, NEXT, InURLPath, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (InURLPathStart, &[r(Any, NEXT, InURLPath, 0)]),
    (
        InURLPath,
        &[
            r(Eof, BINGO, Base, URLPATH),
            r(UrlChar, NEXT, InURLPath, 0),
            r(Any, BINGO, Base, URLPATH),
        ],
    ),
    (
        InFURL,
        &[
            r(Eof, POP, Null, 0),
            rs(UrlPath, BINGO | CLRALL, Base, URL_T, Sp::FUrl),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InProtocolFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'/'), NEXT, InProtocolSecond, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InProtocolSecond,
        &[
            r(Eof, POP, Null, 0),
            r(Eq(b'/'), NEXT, InProtocolEnd, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (InProtocolEnd, &[r(Any, BINGO | CLRALL, Base, PROTOCOL)]),
    (
        InHyphenAsciiWordFirst,
        &[
            r(Eof, POP, Null, 0),
            r(AscLet, NEXT, InHyphenAsciiWord, 0),
            r(Alpha, NEXT, InHyphenWord, 0),
            r(Digit, NEXT, InHyphenDigitLookahead, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHyphenAsciiWord,
        &[
            rs(Eof, BINGO | CLRALL, InParseHyphen, ASCIIHWORD, Sp::Hyphen),
            r(AscLet, NEXT, InHyphenAsciiWord, 0),
            r(Alpha, NEXT, InHyphenWord, 0),
            r(Special, NEXT, InHyphenWord, 0),
            r(Digit, NEXT, InHyphenNumWord, 0),
            r(Eq(b'-'), PUSH, InHyphenAsciiWordFirst, 0),
            rs(Any, BINGO | CLRALL, InParseHyphen, ASCIIHWORD, Sp::Hyphen),
        ],
    ),
    (
        InHyphenWordFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Alpha, NEXT, InHyphenWord, 0),
            r(Digit, NEXT, InHyphenDigitLookahead, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHyphenWord,
        &[
            rs(Eof, BINGO | CLRALL, InParseHyphen, HWORD, Sp::Hyphen),
            r(Alpha, NEXT, InHyphenWord, 0),
            r(Special, NEXT, InHyphenWord, 0),
            r(Digit, NEXT, InHyphenNumWord, 0),
            r(Eq(b'-'), PUSH, InHyphenWordFirst, 0),
            rs(Any, BINGO | CLRALL, InParseHyphen, HWORD, Sp::Hyphen),
        ],
    ),
    (
        InHyphenNumWordFirst,
        &[
            r(Eof, POP, Null, 0),
            r(Alpha, NEXT, InHyphenNumWord, 0),
            r(Digit, NEXT, InHyphenDigitLookahead, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHyphenNumWord,
        &[
            rs(Eof, BINGO | CLRALL, InParseHyphen, NUMHWORD, Sp::Hyphen),
            r(Alnum, NEXT, InHyphenNumWord, 0),
            r(Special, NEXT, InHyphenNumWord, 0),
            r(Eq(b'-'), PUSH, InHyphenNumWordFirst, 0),
            rs(Any, BINGO | CLRALL, InParseHyphen, NUMHWORD, Sp::Hyphen),
        ],
    ),
    (
        InHyphenDigitLookahead,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, InHyphenDigitLookahead, 0),
            r(Alpha, NEXT, InHyphenNumWord, 0),
            r(Special, NEXT, InHyphenNumWord, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InParseHyphen,
        &[
            r(Eof, RERUN, Base, 0),
            r(AscLet, NEXT, InHyphenAsciiWordPart, 0),
            r(Alpha, NEXT, InHyphenWordPart, 0),
            r(Digit, PUSH, InHyphenUnsignedInt, 0),
            r(Eq(b'-'), PUSH, InParseHyphenHyphen, 0),
            r(Any, RERUN, Base, 0),
        ],
    ),
    (
        InParseHyphenHyphen,
        &[
            r(Eof, POP, Null, 0),
            r(Alnum, BINGO | CLEAR, InParseHyphen, SPACE),
            r(Special, BINGO | CLEAR, InParseHyphen, SPACE),
            r(Any, POP, Null, 0),
        ],
    ),
    (
        InHyphenWordPart,
        &[
            r(Eof, BINGO, Base, PARTHWORD),
            r(Alpha, NEXT, InHyphenWordPart, 0),
            r(Special, NEXT, InHyphenWordPart, 0),
            r(Digit, NEXT, InHyphenNumWordPart, 0),
            r(Any, BINGO, InParseHyphen, PARTHWORD),
        ],
    ),
    (
        InHyphenAsciiWordPart,
        &[
            r(Eof, BINGO, Base, ASCIIPARTHWORD),
            r(AscLet, NEXT, InHyphenAsciiWordPart, 0),
            r(Alpha, NEXT, InHyphenWordPart, 0),
            r(Special, NEXT, InHyphenWordPart, 0),
            r(Digit, NEXT, InHyphenNumWordPart, 0),
            r(Any, BINGO, InParseHyphen, ASCIIPARTHWORD),
        ],
    ),
    (
        InHyphenNumWordPart,
        &[
            r(Eof, BINGO, Base, NUMPARTHWORD),
            r(Alnum, NEXT, InHyphenNumWordPart, 0),
            r(Special, NEXT, InHyphenNumWordPart, 0),
            r(Any, BINGO, InParseHyphen, NUMPARTHWORD),
        ],
    ),
    (
        InHyphenUnsignedInt,
        &[
            r(Eof, POP, Null, 0),
            r(Digit, NEXT, Null, 0),
            r(Alpha, CLEAR, InHyphenNumWordPart, 0),
            r(Special, CLEAR, InHyphenNumWordPart, 0),
            r(Any, POP, Null, 0),
        ],
    ),
    (Null, &[]),
];

/// A state's table (`Actions[state].action`).
fn actions(s: St) -> &'static [Item] {
    let (st, items) = ACTIONS[s as usize];
    debug_assert_eq!(st, s);
    items
}

/// One place on the parser's stack (`TParserPosition`): where it stands in bytes and characters,
/// the current character's length, the token so far, its state, and the row it pushed at.
#[derive(Clone, Copy, Debug)]
struct Pos {
    posbyte: usize,
    poschar: usize,
    charlen: usize,
    lenbytetoken: usize,
    lenchartoken: usize,
    state: St,
    pushed_at: Option<usize>,
}

/// The parser (`TParser`): the text in bytes and in code points, the stack of places, the
/// `ignore` and `wanthost` flags, and the last token found. A parser `p_ishost` or `p_isURLPath`
/// starts also keeps the token type its caller waits for; a parser waiting on one keeps the row
/// whose test it is and the flags of the last row it ran.
struct Parser<'a> {
    s: &'a [u8],
    w: &'a [u32],
    stack: Vec<Pos>,
    ignore: bool,
    wanthost: bool,

    token: usize,
    lenbytetoken: usize,
    lenchartoken: usize,
    ty: i32,
    want: i32,
    scan_at: usize,
    last_flags: Option<u8>,
}

/// What a parser started by `p_ishost` or `p_isURLPath` found: whether it found the token type
/// its caller waits for, the token's length in bytes and characters, and the length of the
/// character it stopped at.
#[derive(Clone, Copy, Debug)]
struct Outcome {
    found: bool,
    lenbytetoken: usize,
    lenchartoken: usize,
    charlen: usize,
}

/// Where a parser stands after running: done, with whether it found a token, or waiting on a
/// parser of its own that a test needs.
enum Poll<'a> {
    Ready(bool),
    Pending(Parser<'a>),
}

impl<'a> Parser<'a> {
    /// A parser at the start of `s`, in state `Base` (`TParserInit`, `TParserCopyInit`).
    fn new(s: &'a [u8], w: &'a [u32]) -> Self {
        Parser {
            s,
            w,
            stack: vec![Pos {
                posbyte: 0,
                poschar: 0,
                charlen: 0,
                lenbytetoken: 0,
                lenchartoken: 0,
                state: Base,
                pushed_at: None,
            }],
            ignore: false,
            wanthost: false,

            token: 0,
            lenbytetoken: 0,
            lenchartoken: 0,
            ty: 0,
            want: 0,
            scan_at: 0,
            last_flags: None,
        }
    }

    fn top(&self) -> &Pos {
        self.stack
            .last()
            .expect("the parser's stack is never empty")
    }

    fn top_mut(&mut self) -> &mut Pos {
        self.stack
            .last_mut()
            .expect("the parser's stack is never empty")
    }

    /// The current character as a code point (`prs->pgwstr[prs->state->poschar]`).
    fn wc(&self) -> u32 {
        self.w[self.top().poschar]
    }

    /// The current byte, when the current character is one byte long.
    fn byte1(&self) -> Option<u8> {
        let p = self.top();
        if p.charlen == 1 {
            Some(self.s[p.posbyte])
        } else {
            None
        }
    }

    /// Runs one row's character test (the `p_is*` functions but `p_ishost` and `p_isURLPath`,
    /// which `scan` starts as parsers of their own).
    fn test(&mut self, t: T) -> bool {
        use crate::sys::ctype::Class;
        let class = |c: Class, wc: u32| crate::sys::ctype::is(c, wc);
        match t {
            Eof => {
                let p = self.top();
                p.posbyte == self.s.len() || p.charlen == 0
            }
            Eq(c) => self.byte1() == Some(c),
            Alnum => class(Class::Alnum, self.wc()),
            NotAlnum => !class(Class::Alnum, self.wc()),
            Alpha => class(Class::Alpha, self.wc()),
            Digit => class(Class::Digit, self.wc()),
            Space => class(Class::Space, self.wc()),
            XDigit => class(Class::XDigit, self.wc()),
            AscLet => {
                matches!(self.byte1(), Some(b) if b.is_ascii())
                    && class(Class::Alpha, self.wc())
            }
            Special => self.is_special(),
            Ignore => self.ignore,
            StopHost => {
                if self.wanthost {
                    self.wanthost = false;
                    true
                } else {
                    false
                }
            }
            Host | UrlPath => unreachable!("scan starts a parser for this test"),
            UrlChar => match self.byte1() {
                Some(ch) => {
                    let ch = ch as i8;
                    !(ch <= 0x20 || ch == 0x7f)
                        && !matches!(
                            ch as u8,
                            b'"' | b'<' | b'>' | b'\\' | b'^' | b'`' | b'{' | b'|' | b'}'
                        )
                }
                None => false,
            },
            Any => true,
        }
    }

    /// Whether the current character has no display width, or is one of the spacing combining
    /// marks that do not break a word (`p_isspecial`).
    fn is_special(&self) -> bool {
        let p = self.top();
        if zero_width(utf8_to_unicode(&self.s[p.posbyte..])) {
            return true;
        }
        STRANGE_LETTER.binary_search(&self.wc()).is_ok()
    }

    /// The parser `p_ishost` starts at the current character, wanting a host, and the one
    /// `p_isURLPath` starts there in `InURLPathFirst`, wanting a URL path (`TParserCopyInit`).
    fn sub_parser(&self, t: T) -> Parser<'a> {
        let p = self.top();
        let (s, w) = (self.s, self.w);
        let mut sub = Parser::new(&s[p.posbyte..], &w[p.poschar..]);
        match t {
            Host => {
                sub.wanthost = true;
                sub.want = HOST;
            }
            _ => {
                let mut first = *sub.top();
                first.state = InURLPathFirst;
                sub.stack.push(first);
                sub.want = URLPATH;
            }
        }
        sub
    }

    /// What a parser `sub_parser` started found, once it has run.
    fn outcome(&self, found: bool) -> Outcome {
        Outcome {
            found: found && self.ty == self.want,
            lenbytetoken: self.lenbytetoken,
            lenchartoken: self.lenchartoken,
            charlen: self.top().charlen,
        }
    }

    /// Runs a row's special handler (`SpecialTags`, `SpecialFURL`, `SpecialHyphen`,
    /// `SpecialVerVersion`).
    fn special(&mut self, sp: Sp) {
        match sp {
            Sp::No => {}
            Sp::Tags => {
                let n = self.top().lenchartoken;
                let tok = &self.s[self.token..];
                let starts = |lit: &[u8]| {
                    tok.len() >= lit.len() && tok[..lit.len()].eq_ignore_ascii_case(lit)
                };
                match n {
                    8 => {
                        if starts(b"</script") {
                            self.ignore = false;
                        }
                    }
                    7 => {
                        if starts(b"</style") {
                            self.ignore = false;
                        } else if starts(b"<script") {
                            self.ignore = true;
                        }
                    }
                    6 if starts(b"<style") => {
                        self.ignore = true;
                    }
                    _ => {}
                }
            }
            Sp::FUrl => {
                self.wanthost = true;
                let top = self.top_mut();
                top.posbyte -= top.lenbytetoken;
                top.poschar -= top.lenchartoken;
            }
            Sp::Hyphen => {
                let top = self.top_mut();
                top.posbyte -= top.lenbytetoken;
                top.poschar -= top.lenchartoken;
            }
            Sp::VerVersion => {
                let top = self.top_mut();
                top.posbyte -= top.lenbytetoken;
                top.poschar -= top.lenchartoken;
                top.lenbytetoken = 0;
                top.lenchartoken = 0;
            }
        }
    }

    /// Finds the next token, setting its start, length and type; false when the text has no more
    /// (`TParserGet`). The parsers `p_ishost` and `p_isURLPath` start run on a stack of their own
    /// here rather than on the call stack, however deep they nest.
    fn get(&mut self) -> bool {
        let mut subs: Vec<Parser<'a>> = Vec::new();
        let mut outcome: Option<Outcome> = None;
        loop {
            let poll = match subs.last_mut() {
                Some(sub) => sub.resume(outcome.take()),
                None => self.resume(outcome.take()),
            };
            match poll {
                Poll::Pending(sub) => subs.push(sub),
                Poll::Ready(found) => match subs.pop() {
                    Some(done) => outcome = Some(done.outcome(found)),
                    None => return found,
                },
            }
        }
    }

    /// Finds the index of the first row of the current state's table from `from` whose test
    /// holds (`TParserGet`'s search by character class), or the parser a `p_ishost` or
    /// `p_isURLPath` test at that row starts.
    fn scan(&mut self, from: usize) -> Result<usize, Parser<'a>> {
        let table = actions(self.top().state);
        let mut idx = from;
        loop {
            match table[idx].test {
                t @ (Host | UrlPath) => {
                    self.scan_at = idx;
                    return Err(self.sub_parser(t));
                }
                t => {
                    if self.test(t) {
                        return Ok(idx);
                    }
                    idx += 1;
                }
            }
        }
    }

    /// Runs `TParserGet`'s loop: from the start with no outcome, or with the outcome of the
    /// parser the test at row `scan_at` started, applied as `p_ishost` and `p_isURLPath` apply
    /// it, after which the search goes on.
    fn resume(&mut self, outcome: Option<Outcome>) -> Poll<'a> {
        let len = self.s.len();
        let mut pending: Option<(usize, bool)> = None;
        match outcome {
            None => {
                if self.top().posbyte >= len {
                    return Poll::Ready(false);
                }
                self.token = self.top().posbyte;
                self.top_mut().pushed_at = None;
                self.last_flags = None;
            }
            Some(o) => {
                if o.found {
                    let top = self.top_mut();
                    top.posbyte += o.lenbytetoken;
                    top.poschar += o.lenchartoken;
                    top.lenbytetoken += o.lenbytetoken;
                    top.lenchartoken += o.lenchartoken;
                    top.charlen = o.charlen;
                }
                pending = Some((self.scan_at, o.found));
            }
        }

        loop {
            let found = match pending.take() {
                Some((idx, true)) => Ok(idx),
                Some((idx, false)) => self.scan(idx + 1),
                None => {
                    if self.top().posbyte > len {
                        break;
                    }
                    let pb = self.top().posbyte;
                    let charlen = if pb == len { 0 } else { utf_mblen(self.s[pb]) };
                    assert!(
                        pb + charlen <= len,
                        "invalid byte sequence for encoding \"UTF8\""
                    );
                    self.top_mut().charlen = charlen;
                    let from = match self.top_mut().pushed_at.take() {
                        Some(i) => i + 1,
                        None => 0,
                    };
                    self.scan(from)
                }
            };
            let idx = match found {
                Ok(idx) => idx,
                Err(sub) => return Poll::Pending(sub),
            };
            let item = actions(self.top().state)[idx];
            self.last_flags = Some(item.flags);

            self.special(item.special);

            if item.flags & BINGO != 0 {
                let top = self.top_mut();
                let (lb, lc) = (top.lenbytetoken, top.lenchartoken);
                top.lenbytetoken = 0;
                top.lenchartoken = 0;
                self.lenbytetoken = lb;
                self.lenchartoken = lc;
                self.ty = item.ty;
            }

            if item.flags & POP != 0 {
                self.stack.pop();
                assert!(!self.stack.is_empty(), "the parser popped its last place");
            } else if item.flags & PUSH != 0 {
                self.top_mut().pushed_at = Some(idx);
                let mut next = *self.top();
                next.pushed_at = None;
                self.stack.push(next);
            } else if item.flags & CLEAR != 0 {
                let n = self.stack.len();
                assert!(n >= 2, "the parser cleared a place it does not have");
                self.stack.remove(n - 2);
            } else if item.flags & CLRALL != 0 {
                let top = *self.top();
                self.stack.clear();
                self.stack.push(top);
            } else if item.flags & MERGE != 0 {
                let cur = self.stack.pop().expect("the parser's stack is never empty");
                assert!(
                    !self.stack.is_empty(),
                    "the parser merged into a place it does not have"
                );
                let top = self.top_mut();
                top.posbyte = cur.posbyte;
                top.poschar = cur.poschar;
                top.charlen = cur.charlen;
                top.lenbytetoken = cur.lenbytetoken;
                top.lenchartoken = cur.lenchartoken;
            }

            if item.to != Null {
                self.top_mut().state = item.to;
            }

            if item.flags & BINGO != 0 || (self.top().posbyte >= len && item.flags & RERUN == 0) {
                break;
            }

            if item.flags & (RERUN | POP) != 0 {
                continue;
            }

            let top = self.top_mut();
            if top.charlen != 0 {
                top.posbyte += top.charlen;
                top.lenbytetoken += top.charlen;
                top.poschar += 1;
                top.lenchartoken += 1;
            }
        }

        Poll::Ready(matches!(self.last_flags, Some(f) if f & BINGO != 0))
    }
}

/// The non-spacing characters, as first and last code point of each run
/// (`common/unicode_nonspacing_table.h`).
static NONSPACING: [(u32, u32); 341] = [
    (0x00AD, 0x00AD),
    (0x0300, 0x036F),
    (0x0483, 0x0489),
    (0x0591, 0x05BD),
    (0x05BF, 0x05BF),
    (0x05C1, 0x05C2),
    (0x05C4, 0x05C5),
    (0x05C7, 0x05C7),
    (0x0600, 0x0605),
    (0x0610, 0x061A),
    (0x061C, 0x061C),
    (0x064B, 0x065F),
    (0x0670, 0x0670),
    (0x06D6, 0x06DD),
    (0x06DF, 0x06E4),
    (0x06E7, 0x06E8),
    (0x06EA, 0x06ED),
    (0x070F, 0x070F),
    (0x0711, 0x0711),
    (0x0730, 0x074A),
    (0x07A6, 0x07B0),
    (0x07EB, 0x07F3),
    (0x07FD, 0x07FD),
    (0x0816, 0x0819),
    (0x081B, 0x0823),
    (0x0825, 0x0827),
    (0x0829, 0x082D),
    (0x0859, 0x085B),
    (0x0890, 0x089F),
    (0x08CA, 0x0902),
    (0x093A, 0x093A),
    (0x093C, 0x093C),
    (0x0941, 0x0948),
    (0x094D, 0x094D),
    (0x0951, 0x0957),
    (0x0962, 0x0963),
    (0x0981, 0x0981),
    (0x09BC, 0x09BC),
    (0x09C1, 0x09C4),
    (0x09CD, 0x09CD),
    (0x09E2, 0x09E3),
    (0x09FE, 0x0A02),
    (0x0A3C, 0x0A3C),
    (0x0A41, 0x0A51),
    (0x0A70, 0x0A71),
    (0x0A75, 0x0A75),
    (0x0A81, 0x0A82),
    (0x0ABC, 0x0ABC),
    (0x0AC1, 0x0AC8),
    (0x0ACD, 0x0ACD),
    (0x0AE2, 0x0AE3),
    (0x0AFA, 0x0B01),
    (0x0B3C, 0x0B3C),
    (0x0B3F, 0x0B3F),
    (0x0B41, 0x0B44),
    (0x0B4D, 0x0B56),
    (0x0B62, 0x0B63),
    (0x0B82, 0x0B82),
    (0x0BC0, 0x0BC0),
    (0x0BCD, 0x0BCD),
    (0x0C00, 0x0C00),
    (0x0C04, 0x0C04),
    (0x0C3C, 0x0C3C),
    (0x0C3E, 0x0C40),
    (0x0C46, 0x0C56),
    (0x0C62, 0x0C63),
    (0x0C81, 0x0C81),
    (0x0CBC, 0x0CBC),
    (0x0CBF, 0x0CBF),
    (0x0CC6, 0x0CC6),
    (0x0CCC, 0x0CCD),
    (0x0CE2, 0x0CE3),
    (0x0D00, 0x0D01),
    (0x0D3B, 0x0D3C),
    (0x0D41, 0x0D44),
    (0x0D4D, 0x0D4D),
    (0x0D62, 0x0D63),
    (0x0D81, 0x0D81),
    (0x0DCA, 0x0DCA),
    (0x0DD2, 0x0DD6),
    (0x0E31, 0x0E31),
    (0x0E34, 0x0E3A),
    (0x0E47, 0x0E4E),
    (0x0EB1, 0x0EB1),
    (0x0EB4, 0x0EBC),
    (0x0EC8, 0x0ECE),
    (0x0F18, 0x0F19),
    (0x0F35, 0x0F35),
    (0x0F37, 0x0F37),
    (0x0F39, 0x0F39),
    (0x0F71, 0x0F7E),
    (0x0F80, 0x0F84),
    (0x0F86, 0x0F87),
    (0x0F8D, 0x0FBC),
    (0x0FC6, 0x0FC6),
    (0x102D, 0x1030),
    (0x1032, 0x1037),
    (0x1039, 0x103A),
    (0x103D, 0x103E),
    (0x1058, 0x1059),
    (0x105E, 0x1060),
    (0x1071, 0x1074),
    (0x1082, 0x1082),
    (0x1085, 0x1086),
    (0x108D, 0x108D),
    (0x109D, 0x109D),
    (0x135D, 0x135F),
    (0x1712, 0x1714),
    (0x1732, 0x1733),
    (0x1752, 0x1753),
    (0x1772, 0x1773),
    (0x17B4, 0x17B5),
    (0x17B7, 0x17BD),
    (0x17C6, 0x17C6),
    (0x17C9, 0x17D3),
    (0x17DD, 0x17DD),
    (0x180B, 0x180F),
    (0x1885, 0x1886),
    (0x18A9, 0x18A9),
    (0x1920, 0x1922),
    (0x1927, 0x1928),
    (0x1932, 0x1932),
    (0x1939, 0x193B),
    (0x1A17, 0x1A18),
    (0x1A1B, 0x1A1B),
    (0x1A56, 0x1A56),
    (0x1A58, 0x1A60),
    (0x1A62, 0x1A62),
    (0x1A65, 0x1A6C),
    (0x1A73, 0x1A7F),
    (0x1AB0, 0x1B03),
    (0x1B34, 0x1B34),
    (0x1B36, 0x1B3A),
    (0x1B3C, 0x1B3C),
    (0x1B42, 0x1B42),
    (0x1B6B, 0x1B73),
    (0x1B80, 0x1B81),
    (0x1BA2, 0x1BA5),
    (0x1BA8, 0x1BA9),
    (0x1BAB, 0x1BAD),
    (0x1BE6, 0x1BE6),
    (0x1BE8, 0x1BE9),
    (0x1BED, 0x1BED),
    (0x1BEF, 0x1BF1),
    (0x1C2C, 0x1C33),
    (0x1C36, 0x1C37),
    (0x1CD0, 0x1CD2),
    (0x1CD4, 0x1CE0),
    (0x1CE2, 0x1CE8),
    (0x1CED, 0x1CED),
    (0x1CF4, 0x1CF4),
    (0x1CF8, 0x1CF9),
    (0x1DC0, 0x1DFF),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x20D0, 0x20F0),
    (0x2CEF, 0x2CF1),
    (0x2D7F, 0x2D7F),
    (0x2DE0, 0x2DFF),
    (0x302A, 0x302D),
    (0x3099, 0x309A),
    (0xA66F, 0xA672),
    (0xA674, 0xA67D),
    (0xA69E, 0xA69F),
    (0xA6F0, 0xA6F1),
    (0xA802, 0xA802),
    (0xA806, 0xA806),
    (0xA80B, 0xA80B),
    (0xA825, 0xA826),
    (0xA82C, 0xA82C),
    (0xA8C4, 0xA8C5),
    (0xA8E0, 0xA8F1),
    (0xA8FF, 0xA8FF),
    (0xA926, 0xA92D),
    (0xA947, 0xA951),
    (0xA980, 0xA982),
    (0xA9B3, 0xA9B3),
    (0xA9B6, 0xA9B9),
    (0xA9BC, 0xA9BD),
    (0xA9E5, 0xA9E5),
    (0xAA29, 0xAA2E),
    (0xAA31, 0xAA32),
    (0xAA35, 0xAA36),
    (0xAA43, 0xAA43),
    (0xAA4C, 0xAA4C),
    (0xAA7C, 0xAA7C),
    (0xAAB0, 0xAAB0),
    (0xAAB2, 0xAAB4),
    (0xAAB7, 0xAAB8),
    (0xAABE, 0xAABF),
    (0xAAC1, 0xAAC1),
    (0xAAEC, 0xAAED),
    (0xAAF6, 0xAAF6),
    (0xABE5, 0xABE5),
    (0xABE8, 0xABE8),
    (0xABED, 0xABED),
    (0xFB1E, 0xFB1E),
    (0xFE00, 0xFE0F),
    (0xFE20, 0xFE2F),
    (0xFEFF, 0xFEFF),
    (0xFFF9, 0xFFFB),
    (0x101FD, 0x101FD),
    (0x102E0, 0x102E0),
    (0x10376, 0x1037A),
    (0x10A01, 0x10A0F),
    (0x10A38, 0x10A3F),
    (0x10AE5, 0x10AE6),
    (0x10D24, 0x10D27),
    (0x10D69, 0x10D6D),
    (0x10EAB, 0x10EAC),
    (0x10EFA, 0x10EFF),
    (0x10F46, 0x10F50),
    (0x10F82, 0x10F85),
    (0x11001, 0x11001),
    (0x11038, 0x11046),
    (0x11070, 0x11070),
    (0x11073, 0x11074),
    (0x1107F, 0x11081),
    (0x110B3, 0x110B6),
    (0x110B9, 0x110BA),
    (0x110BD, 0x110BD),
    (0x110C2, 0x110CD),
    (0x11100, 0x11102),
    (0x11127, 0x1112B),
    (0x1112D, 0x11134),
    (0x11173, 0x11173),
    (0x11180, 0x11181),
    (0x111B6, 0x111BE),
    (0x111C9, 0x111CC),
    (0x111CF, 0x111CF),
    (0x1122F, 0x11231),
    (0x11234, 0x11234),
    (0x11236, 0x11237),
    (0x1123E, 0x1123E),
    (0x11241, 0x11241),
    (0x112DF, 0x112DF),
    (0x112E3, 0x112EA),
    (0x11300, 0x11301),
    (0x1133B, 0x1133C),
    (0x11340, 0x11340),
    (0x11366, 0x11374),
    (0x113BB, 0x113C0),
    (0x113CE, 0x113CE),
    (0x113D0, 0x113D0),
    (0x113D2, 0x113D2),
    (0x113E1, 0x113E2),
    (0x11438, 0x1143F),
    (0x11442, 0x11444),
    (0x11446, 0x11446),
    (0x1145E, 0x1145E),
    (0x114B3, 0x114B8),
    (0x114BA, 0x114BA),
    (0x114BF, 0x114C0),
    (0x114C2, 0x114C3),
    (0x115B2, 0x115B5),
    (0x115BC, 0x115BD),
    (0x115BF, 0x115C0),
    (0x115DC, 0x115DD),
    (0x11633, 0x1163A),
    (0x1163D, 0x1163D),
    (0x1163F, 0x11640),
    (0x116AB, 0x116AB),
    (0x116AD, 0x116AD),
    (0x116B0, 0x116B5),
    (0x116B7, 0x116B7),
    (0x1171D, 0x1171D),
    (0x1171F, 0x1171F),
    (0x11722, 0x11725),
    (0x11727, 0x1172B),
    (0x1182F, 0x11837),
    (0x11839, 0x1183A),
    (0x1193B, 0x1193C),
    (0x1193E, 0x1193E),
    (0x11943, 0x11943),
    (0x119D4, 0x119DB),
    (0x119E0, 0x119E0),
    (0x11A01, 0x11A0A),
    (0x11A33, 0x11A38),
    (0x11A3B, 0x11A3E),
    (0x11A47, 0x11A47),
    (0x11A51, 0x11A56),
    (0x11A59, 0x11A5B),
    (0x11A8A, 0x11A96),
    (0x11A98, 0x11A99),
    (0x11B60, 0x11B60),
    (0x11B62, 0x11B64),
    (0x11B66, 0x11B66),
    (0x11C30, 0x11C3D),
    (0x11C3F, 0x11C3F),
    (0x11C92, 0x11CA7),
    (0x11CAA, 0x11CB0),
    (0x11CB2, 0x11CB3),
    (0x11CB5, 0x11CB6),
    (0x11D31, 0x11D45),
    (0x11D47, 0x11D47),
    (0x11D90, 0x11D91),
    (0x11D95, 0x11D95),
    (0x11D97, 0x11D97),
    (0x11EF3, 0x11EF4),
    (0x11F00, 0x11F01),
    (0x11F36, 0x11F3A),
    (0x11F40, 0x11F40),
    (0x11F42, 0x11F42),
    (0x11F5A, 0x11F5A),
    (0x13430, 0x13440),
    (0x13447, 0x13455),
    (0x1611E, 0x16129),
    (0x1612D, 0x1612F),
    (0x16AF0, 0x16AF4),
    (0x16B30, 0x16B36),
    (0x16F4F, 0x16F4F),
    (0x16F8F, 0x16F92),
    (0x16FE4, 0x16FE4),
    (0x1BC9D, 0x1BC9E),
    (0x1BCA0, 0x1BCA3),
    (0x1CF00, 0x1CF46),
    (0x1D167, 0x1D169),
    (0x1D173, 0x1D182),
    (0x1D185, 0x1D18B),
    (0x1D1AA, 0x1D1AD),
    (0x1D242, 0x1D244),
    (0x1DA00, 0x1DA36),
    (0x1DA3B, 0x1DA6C),
    (0x1DA75, 0x1DA75),
    (0x1DA84, 0x1DA84),
    (0x1DA9B, 0x1DAAF),
    (0x1E000, 0x1E02A),
    (0x1E08F, 0x1E08F),
    (0x1E130, 0x1E136),
    (0x1E2AE, 0x1E2AE),
    (0x1E2EC, 0x1E2EF),
    (0x1E4EC, 0x1E4EF),
    (0x1E5EE, 0x1E5EF),
    (0x1E6E3, 0x1E6E3),
    (0x1E6E6, 0x1E6E6),
    (0x1E6EE, 0x1E6EF),
    (0x1E6F5, 0x1E6F5),
    (0x1E8D0, 0x1E8D6),
    (0x1E944, 0x1E94A),
    (0xE0001, 0xE01EF),
];

/// The spacing combining marks `p_isspecial` treats as part of a word, in ascending order
/// (`strange_letter`).
static STRANGE_LETTER: [u32; 228] = [
    0x0903, 0x093E, 0x093F, 0x0940, 0x0949, 0x094A, 0x094B, 0x094C, 0x0982, 0x0983, 0x09BE, 0x09BF,
    0x09C0, 0x09C7, 0x09C8, 0x09CB, 0x09CC, 0x09D7, 0x0A03, 0x0A3E, 0x0A3F, 0x0A40, 0x0A83, 0x0ABE,
    0x0ABF, 0x0AC0, 0x0AC9, 0x0ACB, 0x0ACC, 0x0B02, 0x0B03, 0x0B3E, 0x0B40, 0x0B47, 0x0B48, 0x0B4B,
    0x0B4C, 0x0B57, 0x0BBE, 0x0BBF, 0x0BC1, 0x0BC2, 0x0BC6, 0x0BC7, 0x0BC8, 0x0BCA, 0x0BCB, 0x0BCC,
    0x0BD7, 0x0C01, 0x0C02, 0x0C03, 0x0C41, 0x0C42, 0x0C43, 0x0C44, 0x0C82, 0x0C83, 0x0CBE, 0x0CC0,
    0x0CC1, 0x0CC2, 0x0CC3, 0x0CC4, 0x0CC7, 0x0CC8, 0x0CCA, 0x0CCB, 0x0CD5, 0x0CD6, 0x0D02, 0x0D03,
    0x0D3E, 0x0D3F, 0x0D40, 0x0D46, 0x0D47, 0x0D48, 0x0D4A, 0x0D4B, 0x0D4C, 0x0D57, 0x0D82, 0x0D83,
    0x0DCF, 0x0DD0, 0x0DD1, 0x0DD8, 0x0DD9, 0x0DDA, 0x0DDB, 0x0DDC, 0x0DDD, 0x0DDE, 0x0DDF, 0x0DF2,
    0x0DF3, 0x0F3E, 0x0F3F, 0x0F7F, 0x102B, 0x102C, 0x1031, 0x1038, 0x103B, 0x103C, 0x1056, 0x1057,
    0x1062, 0x1063, 0x1064, 0x1067, 0x1068, 0x1069, 0x106A, 0x106B, 0x106C, 0x106D, 0x1083, 0x1084,
    0x1087, 0x1088, 0x1089, 0x108A, 0x108B, 0x108C, 0x108F, 0x17B6, 0x17BE, 0x17BF, 0x17C0, 0x17C1,
    0x17C2, 0x17C3, 0x17C4, 0x17C5, 0x17C7, 0x17C8, 0x1923, 0x1924, 0x1925, 0x1926, 0x1929, 0x192A,
    0x192B, 0x1930, 0x1931, 0x1933, 0x1934, 0x1935, 0x1936, 0x1937, 0x1938, 0x19B0, 0x19B1, 0x19B2,
    0x19B3, 0x19B4, 0x19B5, 0x19B6, 0x19B7, 0x19B8, 0x19B9, 0x19BA, 0x19BB, 0x19BC, 0x19BD, 0x19BE,
    0x19BF, 0x19C0, 0x19C8, 0x19C9, 0x1A19, 0x1A1A, 0x1A1B, 0x1B04, 0x1B35, 0x1B3B, 0x1B3D, 0x1B3E,
    0x1B3F, 0x1B40, 0x1B41, 0x1B43, 0x1B44, 0x1B82, 0x1BA1, 0x1BA6, 0x1BA7, 0x1BAA, 0x1C24, 0x1C25,
    0x1C26, 0x1C27, 0x1C28, 0x1C29, 0x1C2A, 0x1C2B, 0x1C34, 0x1C35, 0xA823, 0xA824, 0xA827, 0xA880,
    0xA881, 0xA8B4, 0xA8B5, 0xA8B6, 0xA8B7, 0xA8B8, 0xA8B9, 0xA8BA, 0xA8BB, 0xA8BC, 0xA8BD, 0xA8BE,
    0xA8BF, 0xA8C0, 0xA8C1, 0xA8C2, 0xA8C3, 0xA952, 0xA953, 0xAA2F, 0xAA30, 0xAA33, 0xAA34, 0xAA4D,
];

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the port gives exactly the rows PostgreSQL 19 gives for `text`.
    fn check(text: &str, rows: &[(i32, &str)]) {
        let want: Vec<(i32, String)> = rows.iter().map(|&(t, s)| (t, s.to_string())).collect();
        assert_eq!(parse(text), want, "{text:?}");
    }

    #[test]
    fn states_are_in_order() {
        for (i, (st, _)) in ACTIONS.iter().enumerate() {
            assert_eq!(*st as usize, i);
        }
    }

    #[test]
    fn token_types_are_numbered_in_order() {
        for (i, (id, _, _)) in TOKEN_TYPES.iter().enumerate() {
            assert_eq!(*id as usize, i + 1);
        }
    }

    #[test]
    fn nested_emails_need_no_call_stack() {
        let text = format!("{}x.y", "ab1@".repeat(500));
        let rows = std::thread::Builder::new()
            .stack_size(64 << 10)
            .spawn(move || parse(&text))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(rows.len(), 1001);
        for (i, row) in rows[..1000].iter().enumerate() {
            let want = if i % 2 == 0 { (3, "ab1") } else { (12, "@") };
            assert_eq!((row.0, row.1.as_str()), want);
        }
        assert_eq!((rows[1000].0, rows[1000].1.as_str()), (19, "x.y"));
    }

    #[test]
    fn empty_text() {
        check("", &[]);
    }

    #[test]
    fn whitespace_only() {
        check("  \n ", &[(12, "  \n ")]);
    }

    #[test]
    fn ascii_words() {
        check("hello World", &[(1, "hello"), (12, " "), (1, "World")]);
    }

    #[test]
    fn letter_words() {
        check("café naïve", &[(2, "café"), (12, " "), (2, "naïve")]);
    }

    #[test]
    fn words_with_digits() {
        check("abc123 123abc", &[(3, "abc123"), (12, " "), (3, "123abc")]);
    }

    #[test]
    fn emails() {
        check(
            "teodor@stack.net bad@ x@@y.z",
            &[
                (4, "teodor@stack.net"),
                (12, " "),
                (1, "bad"),
                (12, "@ "),
                (1, "x"),
                (12, "@@"),
                (19, "y.z"),
            ],
        );
    }

    #[test]
    fn url_before_a_closing_tag() {
        check(
            "http://www.harewoodsolutions.co.uk/press.aspx</span>",
            &[
                (14, "http://"),
                (5, "www.harewoodsolutions.co.uk/press.aspx"),
                (6, "www.harewoodsolutions.co.uk"),
                (18, "/press.aspx"),
                (13, "</span>"),
            ],
        );
    }

    #[test]
    fn host_port_and_path() {
        check(
            "5aew.werc.ewr:8100/?xx",
            &[
                (5, "5aew.werc.ewr:8100/?xx"),
                (6, "5aew.werc.ewr:8100"),
                (18, "/?xx"),
            ],
        );
    }

    #[test]
    fn hosts() {
        check(
            "aew.werc.ewr www.example.com:80",
            &[(6, "aew.werc.ewr"), (12, " "), (6, "www.example.com:80")],
        );
    }

    #[test]
    fn scientific_notation() {
        check(
            "+4.0e-10 1e5 2E+3",
            &[
                (7, "+4.0e-10"),
                (12, " "),
                (7, "1e5"),
                (12, " "),
                (7, "2E+3"),
            ],
        );
    }

    #[test]
    fn versions() {
        check(
            "1.2.3 -1.2.3 10.20.30.40",
            &[
                (8, "1.2.3"),
                (12, " "),
                (12, "-"),
                (8, "1.2.3"),
                (12, " "),
                (8, "10.20.30.40"),
            ],
        );
    }

    #[test]
    fn ascii_hyphenated_word() {
        check(
            "foo-bar-baz",
            &[
                (16, "foo-bar-baz"),
                (11, "foo"),
                (12, "-"),
                (11, "bar"),
                (12, "-"),
                (11, "baz"),
            ],
        );
    }

    #[test]
    fn letter_hyphenated_word() {
        check(
            "über-cool",
            &[(17, "über-cool"), (10, "über"), (12, "-"), (11, "cool")],
        );
    }

    #[test]
    fn numeric_hyphenated_words() {
        check(
            "abc-123-xyz 1-a a1-b2 abc-12x",
            &[
                (1, "abc"),
                (21, "-123"),
                (12, "-"),
                (1, "xyz"),
                (12, " "),
                (22, "1"),
                (12, "-"),
                (1, "a"),
                (12, " "),
                (15, "a1-b2"),
                (9, "a1"),
                (12, "-"),
                (9, "b2"),
                (12, " "),
                (15, "abc-12x"),
                (11, "abc"),
                (12, "-"),
                (9, "12x"),
            ],
        );
    }

    #[test]
    fn tags() {
        check(
            "<a href=\"x\">text</a><br/>",
            &[
                (13, "<a href=\"x\">"),
                (1, "text"),
                (13, "</a>"),
                (13, "<br/>"),
            ],
        );
    }

    #[test]
    fn comment_processing_instruction_doctype() {
        check(
            "<!-- c --> <?xml version=\"1.0\"?> <!DOCTYPE html>",
            &[
                (13, "<!-- c -->"),
                (12, " "),
                (13, "<?xml version=\"1.0\"?>"),
                (12, " "),
                (13, "<!DOCTYPE html>"),
            ],
        );
    }

    #[test]
    fn script_and_style_bodies_are_blank() {
        check(
            "<script>var x = 1;</script>after <style>p{}</style>z",
            &[
                (13, "<script>"),
                (12, "var x = 1;"),
                (13, "</script>"),
                (1, "after"),
                (12, " "),
                (13, "<style>"),
                (12, "p{}"),
                (13, "</style>"),
                (1, "z"),
            ],
        );
    }

    #[test]
    fn file_paths() {
        check(
            "/usr/local/x.txt ../a/b ~/x",
            &[
                (19, "/usr/local/x.txt"),
                (12, " .."),
                (19, "/a/b"),
                (12, " ~"),
                (19, "/x"),
            ],
        );
    }

    #[test]
    fn decimals() {
        check(
            "234.435 5.005 .5",
            &[
                (20, "234.435"),
                (12, " "),
                (20, "5.005"),
                (12, " ."),
                (22, "5"),
            ],
        );
    }

    #[test]
    fn signed_integers() {
        check("-42 +7", &[(21, "-42"), (12, " "), (21, "+7")]);
    }

    #[test]
    fn unsigned_integers() {
        check("455 007", &[(22, "455"), (12, " "), (22, "007")]);
    }

    #[test]
    fn entities() {
        check(
            "&amp; &#123; &#x1F; &bad &a.b;",
            &[
                (23, "&amp;"),
                (12, " "),
                (23, "&#123;"),
                (12, " "),
                (23, "&#x1F;"),
                (12, " "),
                (12, "&"),
                (1, "bad"),
                (12, " "),
                (23, "&a.b;"),
            ],
        );
    }

    #[test]
    fn double_and_trailing_hyphens() {
        check(
            "x--y a-",
            &[
                (1, "x"),
                (12, "-"),
                (12, "-"),
                (1, "y"),
                (12, " "),
                (1, "a"),
                (12, "-"),
            ],
        );
    }

    #[test]
    fn greek_cjk_cyrillic() {
        check(
            "π-ρ 中文 Привет",
            &[
                (17, "π-ρ"),
                (10, "π"),
                (12, "-"),
                (10, "ρ"),
                (12, " "),
                (2, "中文"),
                (12, " "),
                (2, "Привет"),
            ],
        );
    }

    #[test]
    fn combining_and_spacing_marks() {
        check(
            "ab\u{301}c कि ٣٤",
            &[
                (2, "ab\u{301}c"),
                (12, " "),
                (2, "कि"),
                (12, " "),
                (2, "٣٤"),
            ],
        );
    }

    #[test]
    fn protocols() {
        check(
            "ftp://a.b/c?d#e mailto:me@x.org",
            &[
                (14, "ftp://"),
                (19, "a.b/c"),
                (12, "?"),
                (1, "d"),
                (12, "#"),
                (1, "e"),
                (12, " "),
                (1, "mailto"),
                (12, ":"),
                (4, "me@x.org"),
            ],
        );
    }

    #[test]
    fn escaped_quote_in_tag_and_emoji() {
        check(
            "<a b='it\\'s'> 😀x",
            &[(13, "<a b='it\\'s'>"), (12, " 😀"), (1, "x")],
        );
    }
}
