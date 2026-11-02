// Translated from PostgreSQL's regc_locale.c, whose notice is in NOTICE; this translation is under
// GPL-3.0-or-later.

//! The classes, collating elements and case counterparts (regc_locale.c): the names of ASCII's
//! characters and of the classes, ranges with their other cases, and each class's members by the
//! collation's classes (`locale.rs`).

use super::comp::{Vars, REG_FAKE};
use super::cvec::Cvec;
use super::guts::*;

/// ASCII's characters by name, as `[.name.]` names them (`cnames`).
const CNAMES: &[(&str, u8)] = &[
    ("NUL", 0o0),
    ("SOH", 0o1),
    ("STX", 0o2),
    ("ETX", 0o3),
    ("EOT", 0o4),
    ("ENQ", 0o5),
    ("ACK", 0o6),
    ("BEL", 0o7),
    ("alert", 0o7),
    ("BS", 0o10),
    ("backspace", 0o10),
    ("HT", 0o11),
    ("tab", b'\t'),
    ("LF", 0o12),
    ("newline", b'\n'),
    ("VT", 0o13),
    ("vertical-tab", 0o13),
    ("FF", 0o14),
    ("form-feed", 0o14),
    ("CR", 0o15),
    ("carriage-return", b'\r'),
    ("SO", 0o16),
    ("SI", 0o17),
    ("DLE", 0o20),
    ("DC1", 0o21),
    ("DC2", 0o22),
    ("DC3", 0o23),
    ("DC4", 0o24),
    ("NAK", 0o25),
    ("SYN", 0o26),
    ("ETB", 0o27),
    ("CAN", 0o30),
    ("EM", 0o31),
    ("SUB", 0o32),
    ("ESC", 0o33),
    ("IS4", 0o34),
    ("FS", 0o34),
    ("IS3", 0o35),
    ("GS", 0o35),
    ("IS2", 0o36),
    ("RS", 0o36),
    ("IS1", 0o37),
    ("US", 0o37),
    ("space", b' '),
    ("exclamation-mark", b'!'),
    ("quotation-mark", b'"'),
    ("number-sign", b'#'),
    ("dollar-sign", b'$'),
    ("percent-sign", b'%'),
    ("ampersand", b'&'),
    ("apostrophe", b'\''),
    ("left-parenthesis", b'('),
    ("right-parenthesis", b')'),
    ("asterisk", b'*'),
    ("plus-sign", b'+'),
    ("comma", b','),
    ("hyphen", b'-'),
    ("hyphen-minus", b'-'),
    ("period", b'.'),
    ("full-stop", b'.'),
    ("slash", b'/'),
    ("solidus", b'/'),
    ("zero", b'0'),
    ("one", b'1'),
    ("two", b'2'),
    ("three", b'3'),
    ("four", b'4'),
    ("five", b'5'),
    ("six", b'6'),
    ("seven", b'7'),
    ("eight", b'8'),
    ("nine", b'9'),
    ("colon", b':'),
    ("semicolon", b';'),
    ("less-than-sign", b'<'),
    ("equals-sign", b'='),
    ("greater-than-sign", b'>'),
    ("question-mark", b'?'),
    ("commercial-at", b'@'),
    ("left-square-bracket", b'['),
    ("backslash", b'\\'),
    ("reverse-solidus", b'\\'),
    ("right-square-bracket", b']'),
    ("circumflex", b'^'),
    ("circumflex-accent", b'^'),
    ("underscore", b'_'),
    ("low-line", b'_'),
    ("grave-accent", b'`'),
    ("left-brace", b'{'),
    ("left-curly-bracket", b'{'),
    ("vertical-line", b'|'),
    ("right-brace", b'}'),
    ("right-curly-bracket", b'}'),
    ("tilde", b'~'),
    ("DEL", 0o177),
];

/// The classes by name, in the order of `CharClass` (`classNames`).
const CLASS_NAMES: [&str; NUM_CCLASSES] = [
    "alnum", "alpha", "ascii", "blank", "cntrl", "digit", "graph", "lower", "print", "punct",
    "space", "upper", "xdigit", "word",
];

/// Whether a name of ASCII letters is the characters given.
fn same_name(name: &str, chrs: &[Chr]) -> bool {
    name.len() == chrs.len() && name.bytes().zip(chrs).all(|(b, &c)| b as Chr == c)
}

/// Whether one character comes before another, for a range's order (`before`).
fn before(x: Chr, y: Chr) -> bool {
    x < y
}

impl Vars {
    /// The character a collating element's name stands for: itself where it is one character
    /// (`element`).
    pub(super) fn element(&mut self, name: &[Chr]) -> Chr {
        // one character stands for itself
        if name.len() == 1 {
            return name[0];
        }

        self.note(REG_ULOCALE);

        if let Some(&(_, code)) = CNAMES.iter().find(|(n, _)| same_name(n, name)) {
            return code as Chr;
        }

        self.verr(REG_ECOLLATE);
        0
    }

    /// The characters of a range, `a` to `b`, and where case is ignored, their other cases
    /// outside it (`range`).
    pub(super) fn range(&mut self, a: Chr, b: Chr, cases: bool) -> Option<Cvec> {
        if a != b && !before(a, b) {
            self.verr(REG_ERANGE);
            return None;
        }

        if !cases {
            // the easy one
            let mut cv = self.getcvec(0, 1);
            cv.addrange(a, b);
            return Some(cv);
        }

        // the range itself, then each character's other cases outside it, at most 100000
        let mut nchrs = (b.wrapping_sub(a).wrapping_add(1)) as i32;
        if nchrs <= 0 || nchrs > 100_000 {
            nchrs = 100_000;
        }

        let mut cv = self.getcvec(nchrs as usize, 1);
        cv.addrange(a, b);

        let mut c = a;
        while c <= b {
            let cc = self.ctype.tolower(c);
            if cc != c && (before(cc, a) || before(b, cc)) {
                if cv.chrs.len() >= cv.chrspace {
                    self.verr(REG_ETOOBIG);
                    return None;
                }
                cv.addchr(cc);
            }
            let cc = self.ctype.toupper(c);
            if cc != c && (before(cc, a) || before(b, cc)) {
                if cv.chrs.len() >= cv.chrspace {
                    self.verr(REG_ETOOBIG);
                    return None;
                }
                cv.addchr(cc);
            }
            c += 1;
        }

        Some(cv)
    }

    /// The characters of an equivalence class: the character, with its other cases where case
    /// is ignored (`eclass`).
    pub(super) fn eclass(&mut self, c: Chr, cases: bool) -> Cvec {
        // the test flag's equivalence class
        if (self.cflags & REG_FAKE) != 0 && c == b'x' as Chr {
            let mut cv = self.getcvec(4, 0);
            cv.addchr(b'x' as Chr);
            cv.addchr(b'y' as Chr);
            if cases {
                cv.addchr(b'X' as Chr);
                cv.addchr(b'Y' as Chr);
            }
            return cv;
        }

        // otherwise none
        if cases {
            return self.allcases(c);
        }
        let mut cv = self.getcvec(1, 0);
        cv.addchr(c);
        cv
    }

    /// The class a name names (`lookupcclass`).
    pub(super) fn lookupcclass(&mut self, name: &[Chr]) -> CharClass {
        if let Some(i) = CLASS_NAMES.iter().position(|n| same_name(n, name)) {
            return super::comp::cclass_of(i as Chr);
        }
        self.verr(REG_ECTYPE);
        CharClass::Alnum
    }

    /// The characters of a class; where case is ignored, lower and upper are alpha
    /// (`cclasscvec`).
    pub(super) fn cclasscvec(&mut self, cclasscode: CharClass, cases: bool) -> Cvec {
        use CharClass as K;
        let cclasscode = if cases && (cclasscode == K::Lower || cclasscode == K::Upper) {
            K::Alpha
        } else {
            cclasscode
        };

        match cclasscode {
            K::Print
            | K::Alnum
            | K::Alpha
            | K::Word
            | K::Digit
            | K::Punct
            | K::Space
            | K::Lower
            | K::Upper
            | K::Graph => self.regc_ctype_get_cache(cclasscode),
            K::Ascii => {
                let mut cv = self.getcvec(0, 1);
                cv.addrange(0, 0x7f);
                cv
            }
            K::Blank => {
                let mut cv = self.getcvec(2, 0);
                cv.addchr(b'\t' as Chr);
                cv.addchr(b' ' as Chr);
                cv
            }
            K::Cntrl => {
                let mut cv = self.getcvec(0, 2);
                cv.addrange(0x0, 0x1f);
                cv.addrange(0x7f, 0x9f);
                cv
            }
            K::Xdigit => {
                let mut cv = self.getcvec(0, 3);
                cv.addrange(b'0' as Chr, b'9' as Chr);
                cv.addrange(b'a' as Chr, b'f' as Chr);
                cv.addrange(b'A' as Chr, b'F' as Chr);
                cv
            }
        }
    }

    /// A class's members by the collation's classes, kept once found (`regc_ctype_get_cache`).
    fn regc_ctype_get_cache(&mut self, cls: CharClass) -> Cvec {
        if let Some(cv) = &self.class_cache[cls as usize] {
            return cv.clone();
        }
        let m = self.ctype.class_members(cls);
        let cv = Cvec {
            chrspace: m.chrs.len(),
            chrs: m.chrs,
            ranges: m.ranges,
            cclasscode: if m.locale_dependent { cls as i32 } else { -1 },
        };
        self.class_cache[cls as usize] = Some(cv.clone());
        cv
    }

    /// A character with its other cases (`allcases`).
    pub(super) fn allcases(&mut self, c: Chr) -> Cvec {
        let lc = self.ctype.tolower(c);
        let uc = self.ctype.toupper(c);
        let mut cv = self.getcvec(2, 0);
        cv.addchr(lc);
        if lc != uc {
            cv.addchr(uc);
        }
        cv
    }
}
