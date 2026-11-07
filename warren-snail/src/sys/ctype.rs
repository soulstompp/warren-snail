//! glibc's C.UTF-8 for LC_CTYPE: the character classes and the case mapping libc's C.utf8 and the
//! database's default read, by the C library's own functions, as PostgreSQL calls them.

use crate::{Error, Result};

extern "C" {
    fn iswalnum_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswalpha_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswdigit_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswupper_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswlower_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswgraph_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswprint_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswpunct_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswspace_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn iswxdigit_l(wc: u32, locale: libc::locale_t) -> libc::c_int;
    fn towupper_l(wc: u32, locale: libc::locale_t) -> u32;
    fn towlower_l(wc: u32, locale: libc::locale_t) -> u32;
}

/// A character class of the C library's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    Alnum,
    Alpha,
    Digit,
    Upper,
    Lower,
    Graph,
    Print,
    Punct,
    Space,
    XDigit,
}

/// The locale, opened once and never freed; none where the machine has no C.UTF-8.
fn locale() -> Option<libc::locale_t> {
    static LOCALE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    let at = *LOCALE.get_or_init(|| {
        // SAFETY: newlocale reads the NUL-terminated name and makes a locale of its own.
        let loc = unsafe {
            libc::newlocale(
                libc::LC_CTYPE_MASK,
                c"C.UTF-8".as_ptr(),
                std::ptr::null_mut(),
            )
        };
        loc as usize
    });
    (at != 0).then_some(at as libc::locale_t)
}

/// Refused where the machine's C library has no C.UTF-8, which `what` reads.
pub(crate) fn found(what: &str) -> Result<()> {
    match locale() {
        Some(_) => Ok(()),
        None => Err(Error::new(format!(
            "this machine's C library has no C.UTF-8, which {what}"
        ))),
    }
}

fn held() -> libc::locale_t {
    locale().expect("the machine's C.UTF-8, found before a character was read under it")
}

/// Whether a code point is of a class.
pub(crate) fn is(class: Class, c: u32) -> bool {
    let f = match class {
        Class::Alnum => iswalnum_l,
        Class::Alpha => iswalpha_l,
        Class::Digit => iswdigit_l,
        Class::Upper => iswupper_l,
        Class::Lower => iswlower_l,
        Class::Graph => iswgraph_l,
        Class::Print => iswprint_l,
        Class::Punct => iswpunct_l,
        Class::Space => iswspace_l,
        Class::XDigit => iswxdigit_l,
    };
    // SAFETY: the locale lives as long as the program; any code point is a wint_t, and the
    // classifier only reads it.
    unsafe { f(c, held()) != 0 }
}

/// A code point in upper case.
pub(crate) fn upper(c: u32) -> u32 {
    // SAFETY: as in `is`.
    unsafe { towupper_l(c, held()) }
}

/// A code point in lower case.
pub(crate) fn lower(c: u32) -> u32 {
    // SAFETY: as in `is`.
    unsafe { towlower_l(c, held()) }
}
