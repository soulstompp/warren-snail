//! The character classes and the case mapping a regular expression is compiled and run under, by
//! its collation, as PostgreSQL 19's `regc_pg_locale.c` reads them: under C, POSIX and ucs_basic
//! ASCII's classes alone, from PostgreSQL's own table; under glibc's C.UTF-8 the C library's;
//! under pg_c_utf8 and pg_unicode_fast PostgreSQL's own Unicode tables; under a collation that
//! calls its library, with the feature `collations`, ICU's or glibc's, as the library answers.

use super::guts::{CharClass, Chr, MAX_SIMPLE_CHR};

/// The classes and case mapping of one collation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ctype {
    /// C, POSIX, ucs_basic: ASCII's classes and case alone.
    C,
    /// glibc's C.UTF-8: libc's C.utf8, or the database's default where `default`, which maps
    /// ASCII's letters by C's rules.
    Libc { default: bool },
    /// PostgreSQL's builtin provider: pg_c_utf8, or pg_unicode_fast where `full`, its digit, alnum
    /// and punct Unicode's rather than POSIX's.
    Builtin { full: bool },
    /// A collation that calls its library, ICU or glibc, by the number `collations::get` finds it
    /// by; never one whose LC_CTYPE is C, which reads as `C`.
    Library(u16),
}

const PG_ISDIGIT: u8 = 0x01;
const PG_ISALPHA: u8 = 0x02;
const PG_ISALNUM: u8 = PG_ISDIGIT | PG_ISALPHA;
const PG_ISUPPER: u8 = 0x04;
const PG_ISLOWER: u8 = 0x08;
const PG_ISGRAPH: u8 = 0x10;
const PG_ISPRINT: u8 = 0x20;
const PG_ISPUNCT: u8 = 0x40;
const PG_ISSPACE: u8 = 0x80;

/// ASCII's characters' classes under C (`pg_char_properties`).
fn c_properties(c: Chr) -> u8 {
    let Ok(b) = u8::try_from(c) else { return 0 };
    if b > 127 {
        return 0;
    }
    match b {
        b'\t' | b'\n' | 0x0b | 0x0c | b'\r' => PG_ISSPACE,
        b' ' => PG_ISPRINT | PG_ISSPACE,
        b'0'..=b'9' => PG_ISDIGIT | PG_ISGRAPH | PG_ISPRINT,
        b'A'..=b'Z' => PG_ISALPHA | PG_ISUPPER | PG_ISGRAPH | PG_ISPRINT,
        b'a'..=b'z' => PG_ISALPHA | PG_ISLOWER | PG_ISGRAPH | PG_ISPRINT,
        0x21..=0x7e => PG_ISGRAPH | PG_ISPRINT | PG_ISPUNCT,
        _ => 0,
    }
}

extern "C" {
    fn iswdigit_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswalpha_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswalnum_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswupper_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswlower_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswgraph_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswprint_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswpunct_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswspace_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn towupper_l(wc: u32, locale: libc::locale_t) -> u32;
    fn towlower_l(wc: u32, locale: libc::locale_t) -> u32;
}

/// glibc's C.UTF-8. A snail's regular expressions are compiled under it only once the strings'
/// casing has found it.
fn libc_locale() -> libc::locale_t {
    crate::strings::c_utf8().expect("the machine's C.UTF-8, found before a pattern was compiled")
}

/// The collator of a collation that calls its library.
fn library(id: u16) -> &'static crate::collations::Collator {
    crate::collations::get(id)
}

fn libc_probe(f: unsafe extern "C" fn(u32, libc::locale_t) -> libc::c_int, c: Chr) -> bool {
    // SAFETY: the locale lives as long as the program; any code point is a wint_t.
    unsafe { f(c, libc_locale()) != 0 }
}

impl Ctype {
    /// Whether the classes are C's (`ctype_is_c`).
    pub fn is_c(&self) -> bool {
        matches!(self, Ctype::C)
    }

    /// Whether the C library behind the classes can be read: false where the machine has no
    /// C.UTF-8.
    pub fn available(&self) -> bool {
        match self {
            Ctype::C => true,
            Ctype::Libc { .. } => crate::strings::c_utf8().is_ok(),
            Ctype::Builtin { .. } | Ctype::Library(_) => true,
        }
    }

    fn c_has(c: Chr, bits: u8) -> bool {
        c <= 127 && c_properties(c) & bits != 0
    }

    pub fn isdigit(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISDIGIT),
            Ctype::Libc { .. } => libc_probe(iswdigit_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Digit, c),
            Ctype::Builtin { full } => crate::categories::isdigit(c, !full),
        }
    }

    pub fn isalpha(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISALPHA),
            Ctype::Libc { .. } => libc_probe(iswalpha_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Alpha, c),
            Ctype::Builtin { .. } => crate::categories::isalpha(c),
        }
    }

    pub fn isalnum(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISALNUM),
            Ctype::Libc { .. } => libc_probe(iswalnum_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Alnum, c),
            Ctype::Builtin { full } => crate::categories::isalnum(c, !full),
        }
    }

    /// A word character: alnum, or the underscore (`regc_wc_isword`).
    pub fn isword(&self, c: Chr) -> bool {
        c == Chr::from(b'_') || self.isalnum(c)
    }

    pub fn isupper(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISUPPER),
            Ctype::Libc { .. } => libc_probe(iswupper_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Upper, c),
            Ctype::Builtin { .. } => crate::categories::isupper(c),
        }
    }

    pub fn islower(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISLOWER),
            Ctype::Libc { .. } => libc_probe(iswlower_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Lower, c),
            Ctype::Builtin { .. } => crate::categories::islower(c),
        }
    }

    pub fn isgraph(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISGRAPH),
            Ctype::Libc { .. } => libc_probe(iswgraph_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Graph, c),
            Ctype::Builtin { .. } => crate::categories::isgraph(c),
        }
    }

    pub fn isprint(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISPRINT),
            Ctype::Libc { .. } => libc_probe(iswprint_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Print, c),
            Ctype::Builtin { .. } => crate::categories::isprint(c),
        }
    }

    pub fn ispunct(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISPUNCT),
            Ctype::Libc { .. } => libc_probe(iswpunct_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Punct, c),
            Ctype::Builtin { full } => crate::categories::ispunct(c, !full),
        }
    }

    pub fn isspace(&self, c: Chr) -> bool {
        match self {
            Ctype::C => Self::c_has(c, PG_ISSPACE),
            Ctype::Libc { .. } => libc_probe(iswspace_l, c),
            Ctype::Library(id) => library(*id).is_class(crate::collations::CharClass::Space, c),
            Ctype::Builtin { .. } => crate::categories::isspace(c),
        }
    }

    pub fn toupper(&self, c: Chr) -> Chr {
        match self {
            Ctype::C => ascii_toupper(c),
            Ctype::Libc { default: true } if c <= 127 => ascii_toupper(c),
            // SAFETY: as in `libc_probe`.
            Ctype::Libc { .. } => unsafe { towupper_l(c, libc_locale()) },
            Ctype::Builtin { .. } => crate::strings::simple_case(c, true),
            Ctype::Library(id) => library(*id).char_case(c, true),
        }
    }

    pub fn tolower(&self, c: Chr) -> Chr {
        match self {
            Ctype::C => ascii_tolower(c),
            Ctype::Libc { default: true } if c <= 127 => ascii_tolower(c),
            // SAFETY: as in `libc_probe`.
            Ctype::Libc { .. } => unsafe { towlower_l(c, libc_locale()) },
            Ctype::Builtin { .. } => crate::strings::simple_case(c, false),
            Ctype::Library(id) => library(*id).char_case(c, false),
        }
    }

    /// The probe of a class (`regc_wc_isalpha` and its siblings).
    pub fn probe(&self, cls: CharClass, c: Chr) -> bool {
        use CharClass as K;
        match cls {
            K::Alnum => self.isalnum(c),
            K::Alpha => self.isalpha(c),
            K::Digit => self.isdigit(c),
            K::Graph => self.isgraph(c),
            K::Lower => self.islower(c),
            K::Print => self.isprint(c),
            K::Punct => self.ispunct(c),
            K::Space => self.isspace(c),
            K::Upper => self.isupper(c),
            K::Word => self.isword(c),
            // regc_locale.c builds these itself and never probes them.
            K::Ascii | K::Blank | K::Cntrl | K::Xdigit => false,
        }
    }

    /// The members of a class up to the last character probed, as `regc_ctype_get_cache` stores
    /// them: runs of one character apart, longer runs as ranges; and whether characters past them
    /// still ask the class at run time (under C they do not, its members being ASCII's).
    pub fn class_members(&self, cls: CharClass) -> ClassMembers {
        let (max_chr, locale_dependent) = if self.is_c() {
            (127, false)
        } else {
            (MAX_SIMPLE_CHR, true)
        };
        let mut out = ClassMembers {
            chrs: Vec::new(),
            ranges: Vec::new(),
            locale_dependent,
        };
        let mut store = |first: Chr, n: Chr| {
            if n > 1 {
                out.ranges.push((first, first + n - 1));
            } else {
                out.chrs.push(first);
            }
        };
        let mut nmatches: Chr = 0;
        let mut cur: Chr = 0;
        while cur <= max_chr {
            if self.probe(cls, cur) {
                nmatches += 1;
            } else if nmatches > 0 {
                store(cur - nmatches, nmatches);
                nmatches = 0;
            }
            cur += 1;
        }
        if nmatches > 0 {
            store(cur - nmatches, nmatches);
        }
        out
    }
}

/// A class's members as `regc_ctype_get_cache` gives them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClassMembers {
    pub chrs: Vec<Chr>,
    pub ranges: Vec<(Chr, Chr)>,
    /// Whether the cvec keeps the class's code (`cclasscode`), so characters past the last probed
    /// ask it at run time; under C it is -1.
    pub locale_dependent: bool,
}

fn ascii_toupper(c: Chr) -> Chr {
    if (Chr::from(b'a')..=Chr::from(b'z')).contains(&c) {
        c - 32
    } else {
        c
    }
}

fn ascii_tolower(c: Chr) -> Chr {
    if (Chr::from(b'A')..=Chr::from(b'Z')).contains(&c) {
        c + 32
    } else {
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_s_classes_are_ascii_s_alone() {
        let c = Ctype::C;
        assert!(c.isalpha('a' as Chr) && c.isupper('Q' as Chr) && c.isdigit('7' as Chr));
        assert!(!c.isalpha(0xe9) && !c.isprint(0x7f) && c.isspace(0x0b));
        assert!(c.ispunct('_' as Chr) && c.isword('_' as Chr) && !c.isgraph(' ' as Chr));
        assert_eq!((c.toupper(0xe9), c.tolower('Z' as Chr)), (0xe9, 'z' as Chr));
        let digits = c.class_members(CharClass::Digit);
        assert_eq!((digits.chrs, digits.ranges), (vec![], vec![(48, 57)]));
        assert!(!digits.locale_dependent);
    }

    #[test]
    fn c_utf8_s_classes_are_the_c_library_s() {
        let t = Ctype::Libc { default: true };
        if !t.available() {
            return;
        }
        assert!(t.isalpha(0xe9) && t.isupper(0xc9) && t.tolower(0xc9) == 0xe9);
        let alpha = t.class_members(CharClass::Alpha);
        assert!(alpha.locale_dependent && alpha.ranges.contains(&(65, 90)));
    }

    #[test]
    fn the_builtin_provider_s_classes_are_postgresql_s_own_tables() {
        let (posix, full) = (
            Ctype::Builtin { full: false },
            Ctype::Builtin { full: true },
        );
        assert!(!posix.isdigit(0x0663) && full.isdigit(0x0663));
        assert!(posix.ispunct('$' as Chr) && !full.ispunct('$' as Chr));
        assert!(
            posix.isalpha(0x1500) && posix.tolower(0xc9) == 0xe9 && posix.toupper(0x1f0) == 0x1f0
        );
        assert!(posix.class_members(CharClass::Space).locale_dependent);
    }
}
