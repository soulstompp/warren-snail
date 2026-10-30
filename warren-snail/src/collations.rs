//! Text under a collation that orders it otherwise than by its bytes: an ICU collator, or a glibc
//! locale, each called as PostgreSQL 19 calls it (`pg_locale_icu.c`, `pg_locale_libc.c`). Built
//! only with the feature `collations`, which links the machine's ICU; without it such a collation is
//! named and refused wherever its order or its case is asked.

use crate::{Error, Result};
use std::cmp::Ordering;

/// The library a collation calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Provider {
    Icu,
    Libc,
}

impl Provider {
    pub fn name(self) -> &'static str {
        match self {
            Provider::Icu => "icu",
            Provider::Libc => "libc",
        }
    }
}

/// A collation that calls a library, as PostgreSQL's catalog holds it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct Spec {
    pub provider: Provider,
    /// ICU's locale, or glibc's LC_COLLATE.
    pub locale: String,
    /// glibc's LC_CTYPE; ICU's locale again.
    pub ctype: String,
    /// ICU's rules, added to its locale's own.
    pub rules: Option<String>,
    pub deterministic: bool,
}

impl Spec {
    pub fn icu(locale: &str, rules: Option<&str>, deterministic: bool) -> Spec {
        Spec {
            provider: Provider::Icu,
            locale: locale.into(),
            ctype: locale.into(),
            rules: rules.map(Into::into),
            deterministic,
        }
    }

    pub fn libc(collate: &str, ctype: &str) -> Spec {
        Spec {
            provider: Provider::Libc,
            locale: collate.into(),
            ctype: ctype.into(),
            rules: None,
            deterministic: true,
        }
    }

    /// Whether glibc's LC_COLLATE is C or POSIX, which PostgreSQL compares by the bytes.
    pub fn collate_is_c(&self) -> bool {
        self.provider == Provider::Libc && matches!(self.locale.as_str(), "C" | "POSIX")
    }

    /// What an order cut under it records: its provider, locale, rules and determinism, and the
    /// version of its order the library gives.
    pub fn cut_under(&self, version: &str) -> String {
        let mut s = format!("collation {} {}", self.provider.name(), self.locale);
        if self.ctype != self.locale {
            s.push_str(&format!(" ctype {}", self.ctype));
        }
        if let Some(r) = &self.rules {
            s.push_str(&format!(" rules {r:?}"));
        }
        if !self.deterministic {
            s.push_str(" nondeterministic");
        }
        s.push_str(&format!(" version {version}"));
        s
    }
}

/// Why a collation that calls a library is refused by a build without the feature.
pub(crate) fn unbuilt(name: &str) -> Error {
    Error::new(format!(
        "collation \"{name}\" orders text as its library does, and this warren-snail is built \
         without the feature collations"
    ))
}

#[cfg(not(feature = "collations"))]
mod library {
    use super::*;

    #[non_exhaustive]
    pub struct Collator {
        pub spec: Spec,
    }

    impl Collator {
        pub fn key(&self, _s: &[u8]) -> Vec<u8> {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn cmp(&self, _a: &[u8], _b: &[u8]) -> Ordering {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn agrees(&self, _a: &[u8], _b: &[u8]) -> bool {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn equal(&self, _a: &[u8], _b: &[u8]) -> bool {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn case(&self, _s: &str, _upper: bool) -> Result<String> {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn version(&self) -> &str {
            unreachable!("no collator opens without the feature collations")
        }
    }

    pub fn open(spec: &Spec, name: &str) -> Result<(u16, &'static Collator)> {
        let _ = spec;
        Err(unbuilt(name))
    }

    pub fn get(_id: u16) -> &'static Collator {
        unreachable!("no collator opens without the feature collations")
    }

    pub fn language_tag(_locale: &str) -> Result<String> {
        Err(Error::new(
            "ICU is not supported in this build: warren-snail is built without the feature \
             collations",
        ))
    }

    pub fn icu_locales() -> Vec<String> {
        Vec::new()
    }

    pub fn libc_locale(_name: &str) -> Option<String> {
        None
    }

    pub fn icu_version() -> Option<String> {
        None
    }
}

#[cfg(feature = "collations")]
mod library {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::{c_char, CStr, CString};
    use std::sync::{Mutex, OnceLock};

    #[repr(C)]
    pub struct UCollator {
        _private: [u8; 0],
    }

    #[repr(C)]
    pub struct UCaseMap {
        _private: [u8; 0],
    }

    type UErrorCode = i32;
    const U_ZERO_ERROR: UErrorCode = 0;
    const U_BUFFER_OVERFLOW_ERROR: UErrorCode = 15;
    const U_STRING_NOT_TERMINATED_WARNING: UErrorCode = -124;
    const UCOL_DEFAULT: i32 = -1;
    const U_FOLD_CASE_DEFAULT: u32 = 0;
    const U_FOLD_CASE_EXCLUDE_SPECIAL_I: u32 = 1;
    const ULOC_LANG_CAPACITY: usize = 12;
    const U_MAX_VERSION_STRING_LENGTH: usize = 20;

    macro_rules! icu {
        ($($name:ident ( $($a:ident : $t:ty),* ) $(-> $r:ty)? ;)*) => {
            extern "C" {
                $(
                    #[link_name = concat!(stringify!($name), env!("WARREN_SNAIL_ICU_SUFFIX"))]
                    fn $name($($a: $t),*) $(-> $r)?;
                )*
            }
        };
    }

    icu! {
        ucol_open(loc: *const c_char, status: *mut UErrorCode) -> *mut UCollator;
        ucol_openRules(
            rules: *const u16,
            length: i32,
            normalization: i32,
            strength: i32,
            parse_error: *mut u8,
            status: *mut UErrorCode
        ) -> *mut UCollator;
        ucol_close(coll: *mut UCollator);
        ucol_getRules(coll: *const UCollator, length: *mut i32) -> *const u16;
        ucol_strcollUTF8(
            coll: *const UCollator,
            source: *const c_char,
            source_length: i32,
            target: *const c_char,
            target_length: i32,
            status: *mut UErrorCode
        ) -> i32;
        ucol_getSortKey(
            coll: *const UCollator,
            source: *const u16,
            source_length: i32,
            result: *mut u8,
            result_length: i32
        ) -> i32;
        ucol_getVersion(coll: *const UCollator, info: *mut u8);
        u_versionToString(info: *const u8, out: *mut c_char);
        u_getVersion(info: *mut u8);
        u_errorName(code: UErrorCode) -> *const c_char;
        ucasemap_open(locale: *const c_char, options: u32, status: *mut UErrorCode) -> *mut UCaseMap;
        ucasemap_utf8ToLower(
            csm: *const UCaseMap,
            dest: *mut c_char,
            capacity: i32,
            src: *const c_char,
            length: i32,
            status: *mut UErrorCode
        ) -> i32;
        ucasemap_utf8ToUpper(
            csm: *const UCaseMap,
            dest: *mut c_char,
            capacity: i32,
            src: *const c_char,
            length: i32,
            status: *mut UErrorCode
        ) -> i32;
        uloc_getLanguage(
            locale: *const c_char,
            language: *mut c_char,
            capacity: i32,
            status: *mut UErrorCode
        ) -> i32;
        uloc_toLanguageTag(
            locale: *const c_char,
            tag: *mut c_char,
            capacity: i32,
            strict: i8,
            status: *mut UErrorCode
        ) -> i32;
        uloc_countAvailable() -> i32;
        uloc_getAvailable(n: i32) -> *const c_char;
    }

    extern "C" {
        fn strcoll_l(a: *const c_char, b: *const c_char, loc: libc::locale_t) -> libc::c_int;
        fn strxfrm_l(
            dest: *mut c_char,
            src: *const c_char,
            n: libc::size_t,
            loc: libc::locale_t,
        ) -> libc::size_t;
        fn towlower_l(wc: u32, locale: libc::locale_t) -> u32;
        fn towupper_l(wc: u32, locale: libc::locale_t) -> u32;
    }

    fn failed(status: UErrorCode) -> bool {
        status > U_ZERO_ERROR
    }

    fn error_name(status: UErrorCode) -> String {
        // SAFETY: u_errorName returns a static string for any code.
        unsafe { CStr::from_ptr(u_errorName(status)) }
            .to_string_lossy()
            .into_owned()
    }

    fn c_string(s: &str) -> Result<CString> {
        CString::new(s).map_err(|_| Error::new(format!("{s:?} holds a NUL")))
    }

    enum Inner {
        Icu {
            coll: *mut UCollator,
            casemap: *mut UCaseMap,
        },
        Libc {
            loc: libc::locale_t,
        },
    }

    /// A collation opened: its library's collator or locale, and the version of its order.
    #[non_exhaustive]
    pub struct Collator {
        pub spec: Spec,
        version: String,
        inner: Inner,
    }

    // SAFETY: ICU's collators and case maps, and glibc's locale_t, may be used from any thread,
    // though by one thread at a time; each thread opens and keeps its own (`at`).
    unsafe impl Send for Collator {}

    /// The collations opened, by the number `open` gives each: the same on every thread.
    static SPECS: Mutex<Vec<Spec>> = Mutex::new(Vec::new());

    std::thread_local! {
        /// This thread's collator of each collation opened, as a backend keeps the locales it
        /// builds, kept as long as the program runs.
        static HELD: RefCell<Vec<Option<&'static Collator>>> = const { RefCell::new(Vec::new()) };
    }

    /// The collator of `spec` on this thread, with the number it is found by on any. `name` names
    /// it in a refusal.
    pub fn open(spec: &Spec, name: &str) -> Result<(u16, &'static Collator)> {
        let id = {
            let mut specs = SPECS.lock().unwrap();
            match specs.iter().position(|s| s == spec) {
                Some(at) => at,
                None => {
                    let c = make(spec, name)?;
                    specs.push(spec.clone());
                    let id = specs.len() - 1;
                    HELD.with_borrow_mut(|h| {
                        h.resize(id + 1, None);
                        h[id] = Some(Box::leak(Box::new(c)));
                    });
                    id
                }
            }
        };
        Ok((id as u16, at(id, name)?))
    }

    /// This thread's collator of the collation numbered `id`, opened here the first time.
    fn at(id: usize, name: &str) -> Result<&'static Collator> {
        if let Some(c) = HELD.with_borrow(|h| h.get(id).copied().flatten()) {
            return Ok(c);
        }
        let spec = SPECS.lock().unwrap()[id].clone();
        let c: &'static Collator = Box::leak(Box::new(make(&spec, name)?));
        HELD.with_borrow_mut(|h| {
            if h.len() <= id {
                h.resize(id + 1, None);
            }
            h[id] = Some(c);
        });
        Ok(c)
    }

    /// The collator `open` gave the number `id`, on this thread.
    pub fn get(id: u16) -> &'static Collator {
        at(id as usize, "")
            .expect("a collation opened on one thread opens on another, its library the same")
    }

    fn make(spec: &Spec, name: &str) -> Result<Collator> {
        match spec.provider {
            Provider::Icu => {
                let coll = icu_collator(&spec.locale, spec.rules.as_deref())?;
                let mut info = [0u8; 4];
                let mut text = [0 as c_char; U_MAX_VERSION_STRING_LENGTH];
                // SAFETY: the collator is open, and both buffers hold what ICU writes.
                unsafe {
                    ucol_getVersion(coll, info.as_mut_ptr());
                    u_versionToString(info.as_ptr(), text.as_mut_ptr());
                }
                let version = unsafe { CStr::from_ptr(text.as_ptr()) }
                    .to_string_lossy()
                    .into_owned();
                let loc = c_string(&spec.locale)?;
                let mut status = U_ZERO_ERROR;
                // SAFETY: the locale is a C string; status is ICU's to write.
                let casemap =
                    unsafe { ucasemap_open(loc.as_ptr(), fold_options(&spec.locale), &mut status) };
                if failed(status) {
                    // SAFETY: the collator was opened above and is not kept.
                    unsafe { ucol_close(coll) };
                    return Err(Error::new(format!(
                        "could not open casemap for locale \"{}\": {}",
                        spec.locale,
                        error_name(status)
                    )));
                }
                Ok(Collator {
                    spec: spec.clone(),
                    version,
                    inner: Inner::Icu { coll, casemap },
                })
            }
            Provider::Libc => {
                let loc = libc_locale_t(&spec.locale, &spec.ctype, name)?;
                Ok(Collator {
                    spec: spec.clone(),
                    version: libc_version(),
                    inner: Inner::Libc { loc },
                })
            }
        }
    }

    /// An ICU collator as `make_icu_collator` opens one: the locale's own, or with rules, the
    /// locale's rules and these after them.
    fn icu_collator(locale: &str, rules: Option<&str>) -> Result<*mut UCollator> {
        let loc = c_string(locale)?;
        let mut status = U_ZERO_ERROR;
        // SAFETY: the locale is a C string; status is ICU's to write.
        let coll = unsafe { ucol_open(loc.as_ptr(), &mut status) };
        if failed(status) {
            return Err(Error::new(format!(
                "could not open collator for locale \"{locale}\": {}",
                error_name(status)
            )));
        }
        let Some(rules) = rules else {
            return Ok(coll);
        };
        let mut len = 0i32;
        // SAFETY: the collator is open; ICU gives its rules and their length, held by it.
        let own = unsafe { ucol_getRules(coll, &mut len) };
        let mut all: Vec<u16> = if own.is_null() || len <= 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(own, len as usize) }.to_vec()
        };
        all.extend(rules.encode_utf16());
        // SAFETY: the rules are copied out before the collator they came from is closed.
        unsafe { ucol_close(coll) };
        let mut status = U_ZERO_ERROR;
        // SAFETY: the rules are UTF-16 of the length given; no parse error is asked for.
        let coll = unsafe {
            ucol_openRules(
                all.as_ptr(),
                all.len() as i32,
                UCOL_DEFAULT,
                UCOL_DEFAULT,
                std::ptr::null_mut(),
                &mut status,
            )
        };
        if failed(status) {
            return Err(Error::new(format!(
                "could not open collator for locale \"{locale}\" with rules \"{rules}\": {}",
                error_name(status)
            )));
        }
        Ok(coll)
    }

    /// The options ICU's case folding takes for a locale: Turkic for tr and az.
    fn fold_options(locale: &str) -> u32 {
        let Ok(loc) = CString::new(locale) else {
            return U_FOLD_CASE_DEFAULT;
        };
        let mut lang = [0 as c_char; ULOC_LANG_CAPACITY];
        let mut status = U_ZERO_ERROR;
        // SAFETY: the buffer holds ULOC_LANG_CAPACITY bytes, as given.
        unsafe {
            uloc_getLanguage(
                loc.as_ptr(),
                lang.as_mut_ptr(),
                ULOC_LANG_CAPACITY as i32,
                &mut status,
            )
        };
        if failed(status) || status == U_STRING_NOT_TERMINATED_WARNING {
            return U_FOLD_CASE_DEFAULT;
        }
        match unsafe { CStr::from_ptr(lang.as_ptr()) }.to_bytes() {
            b"tr" | b"az" => U_FOLD_CASE_EXCLUDE_SPECIAL_I,
            _ => U_FOLD_CASE_DEFAULT,
        }
    }

    /// A glibc locale as `make_libc_collator` makes one: LC_COLLATE and LC_CTYPE at once where they
    /// are the same, in two steps where not, C and POSIX left to the bytes.
    fn libc_locale_t(collate: &str, ctype: &str, name: &str) -> Result<libc::locale_t> {
        let is_c = |s: &str| matches!(s, "C" | "POSIX");
        let new = |mask: libc::c_int, l: &str, base: libc::locale_t| -> Result<libc::locale_t> {
            let c = c_string(l)?;
            // SAFETY: the name is a C string; a base of null or one newlocale made is glibc's.
            let loc = unsafe { libc::newlocale(mask, c.as_ptr(), base) };
            if loc.is_null() {
                return Err(Error::new(format!(
                    "could not create locale \"{l}\" for collation \"{name}\""
                )));
            }
            Ok(loc)
        };
        let mut loc: libc::locale_t = std::ptr::null_mut();
        if collate == ctype {
            if !is_c(collate) {
                loc = new(libc::LC_COLLATE_MASK | libc::LC_CTYPE_MASK, collate, loc)?;
            }
        } else {
            if !is_c(collate) {
                loc = new(libc::LC_COLLATE_MASK, collate, loc)?;
            }
            if !is_c(ctype) {
                loc = new(libc::LC_CTYPE_MASK, ctype, loc)?;
            }
        }
        if loc.is_null() {
            // SAFETY: "C" is a C string; glibc's C locale is always made.
            loc =
                unsafe { libc::newlocale(libc::LC_ALL_MASK, c"C".as_ptr(), std::ptr::null_mut()) };
        }
        Ok(loc)
    }

    fn libc_version() -> String {
        // SAFETY: glibc returns a static string.
        unsafe { CStr::from_ptr(libc::gnu_get_libc_version()) }
            .to_string_lossy()
            .into_owned()
    }

    impl Collator {
        /// The bytes `s` is keyed by under this collation: ICU's sort key, or glibc's `strxfrm_l`
        /// bytes and a NUL, then, where the collation is deterministic, `s` itself.
        pub fn key(&self, s: &[u8]) -> Vec<u8> {
            let mut out = match &self.inner {
                Inner::Icu { coll, .. } => {
                    let text = String::from_utf8_lossy(s);
                    let u: Vec<u16> = text.encode_utf16().collect();
                    let mut buf = vec![0u8; 4 * u.len() + 16];
                    loop {
                        // SAFETY: the collator is open; the source and result lengths are theirs.
                        let n = unsafe {
                            ucol_getSortKey(
                                *coll,
                                u.as_ptr(),
                                u.len() as i32,
                                buf.as_mut_ptr(),
                                buf.len() as i32,
                            )
                        } as usize;
                        if n <= buf.len() {
                            buf.truncate(n);
                            break;
                        }
                        buf.resize(n, 0);
                    }
                    buf
                }
                Inner::Libc { loc } => {
                    if self.spec.collate_is_c() {
                        return s.to_vec();
                    }
                    let src = CString::new(s).unwrap_or_default();
                    let mut buf = vec![0u8; 4 * s.len() + 16];
                    loop {
                        // SAFETY: the source is a C string; glibc writes at most `buf.len()` bytes.
                        let n = unsafe {
                            strxfrm_l(
                                buf.as_mut_ptr() as *mut c_char,
                                src.as_ptr(),
                                buf.len(),
                                *loc,
                            )
                        };
                        if n < buf.len() {
                            buf.truncate(n);
                            break;
                        }
                        buf.resize(n + 1, 0);
                    }
                    buf.push(0);
                    buf
                }
            };
            if self.spec.deterministic {
                out.extend_from_slice(s);
            }
            out
        }

        /// How two texts compare under this collation, as `varstr_cmp` compares them: equal bytes
        /// equal; then the library; then, where it calls them equal and the collation is
        /// deterministic, their bytes.
        pub fn cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
            if a == b {
                return Ordering::Equal;
            }
            let r = self.library_cmp(a, b);
            if r == Ordering::Equal && self.spec.deterministic {
                return a.cmp(b);
            }
            r
        }

        fn library_cmp(&self, a: &[u8], b: &[u8]) -> Ordering {
            match &self.inner {
                Inner::Icu { coll, .. } => {
                    let mut status = U_ZERO_ERROR;
                    // SAFETY: the collator is open; each text is given with its length.
                    let r = unsafe {
                        ucol_strcollUTF8(
                            *coll,
                            a.as_ptr() as *const c_char,
                            a.len() as i32,
                            b.as_ptr() as *const c_char,
                            b.len() as i32,
                            &mut status,
                        )
                    };
                    r.cmp(&0)
                }
                Inner::Libc { loc } => {
                    if self.spec.collate_is_c() {
                        return a.cmp(b);
                    }
                    let x = CString::new(a).unwrap_or_default();
                    let y = CString::new(b).unwrap_or_default();
                    // SAFETY: both are C strings; the locale is glibc's.
                    unsafe { strcoll_l(x.as_ptr(), y.as_ptr(), *loc) }.cmp(&0)
                }
            }
        }

        /// Whether the library calls two texts equal, its ties left unbroken, as `pg_strncoll`
        /// compares them.
        pub fn equal(&self, a: &[u8], b: &[u8]) -> bool {
            self.library_cmp(a, b) == Ordering::Equal
        }

        /// Whether the keys of two texts compare as the collation compares them.
        pub fn agrees(&self, a: &[u8], b: &[u8]) -> bool {
            self.key(a).cmp(&self.key(b)) == self.cmp(a, b)
        }

        /// A text in upper case, or in lower case where `!upper`, as PostgreSQL's upper and lower
        /// map it under this collation: ICU's case map of its locale, or glibc's towupper_l and
        /// towlower_l a character at a time.
        pub fn case(&self, s: &str, upper: bool) -> Result<String> {
            match &self.inner {
                Inner::Icu { casemap, .. } => {
                    let mut buf = vec![0u8; s.len() + 16];
                    loop {
                        let mut status = U_ZERO_ERROR;
                        // SAFETY: the case map is open; the source and destination lengths are
                        // theirs.
                        let n = unsafe {
                            let f = if upper {
                                ucasemap_utf8ToUpper
                            } else {
                                ucasemap_utf8ToLower
                            };
                            f(
                                *casemap,
                                buf.as_mut_ptr() as *mut c_char,
                                buf.len() as i32,
                                s.as_ptr() as *const c_char,
                                s.len() as i32,
                                &mut status,
                            )
                        } as usize;
                        if status == U_BUFFER_OVERFLOW_ERROR || n > buf.len() {
                            buf.resize(n + 1, 0);
                            continue;
                        }
                        if failed(status) {
                            return Err(Error::new(format!(
                                "case conversion failed: {}",
                                error_name(status)
                            )));
                        }
                        buf.truncate(n);
                        return String::from_utf8(buf)
                            .map_err(|_| Error::new("case conversion gave no UTF-8"));
                    }
                }
                Inner::Libc { loc } => Ok(s
                    .chars()
                    .map(|ch| {
                        // SAFETY: the locale lives as long as the program; any code point is a
                        // wint_t.
                        let m = unsafe {
                            if upper {
                                towupper_l(ch as u32, *loc)
                            } else {
                                towlower_l(ch as u32, *loc)
                            }
                        };
                        char::from_u32(m).unwrap_or(ch)
                    })
                    .collect()),
            }
        }

        /// The version of its order its library gives, as PostgreSQL records it: the collator's
        /// under ICU, glibc's own under libc.
        pub fn version(&self) -> &str {
            &self.version
        }
    }

    /// An ICU locale as the language tag PostgreSQL stores it under, strict, as
    /// `icu_language_tag` makes it.
    pub fn language_tag(locale: &str) -> Result<String> {
        let loc = c_string(locale)?;
        let mut cap = 32usize;
        loop {
            let mut buf = vec![0 as c_char; cap];
            let mut status = U_ZERO_ERROR;
            // SAFETY: the buffer holds `cap` bytes, as given.
            unsafe {
                uloc_toLanguageTag(loc.as_ptr(), buf.as_mut_ptr(), cap as i32, 1, &mut status)
            };
            if (status == U_BUFFER_OVERFLOW_ERROR || status == U_STRING_NOT_TERMINATED_WARNING)
                && cap < 1 << 20
            {
                cap *= 2;
                continue;
            }
            if failed(status) {
                return Err(Error::new(format!(
                    "could not convert locale name \"{locale}\" to language tag: {}",
                    error_name(status)
                )));
            }
            return Ok(unsafe { CStr::from_ptr(buf.as_ptr()) }
                .to_string_lossy()
                .into_owned());
        }
    }

    /// The language tags of the locales ICU holds, the root first, as initdb names its `-x-icu`
    /// collations: ASCII only.
    pub fn icu_locales() -> Vec<String> {
        static TAGS: OnceLock<Vec<String>> = OnceLock::new();
        TAGS.get_or_init(|| {
            // SAFETY: ICU counts its locales, and names each by a static string.
            let n = unsafe { uloc_countAvailable() };
            let mut out = Vec::new();
            for i in -1..n {
                let name = if i < 0 {
                    String::new()
                } else {
                    unsafe { CStr::from_ptr(uloc_getAvailable(i)) }
                        .to_string_lossy()
                        .into_owned()
                };
                if let Ok(t) = language_tag(&name) {
                    if t.is_ascii() {
                        out.push(t);
                    }
                }
            }
            out
        })
        .clone()
    }

    /// The LC_COLLATE PostgreSQL's catalog gives a libc collation of this name on this machine, as
    /// initdb imports them from the locales `locale -a` names: the name, where glibc opens it as a
    /// UTF-8 locale; or the name and `.utf8`, where the name has no encoding of its own.
    pub fn libc_locale(name: &str) -> Option<String> {
        let utf8 = |l: &str| -> bool {
            let Ok(c) = CString::new(l) else {
                return false;
            };
            // SAFETY: the name is a C string; a locale made here is freed here.
            unsafe {
                let loc = libc::newlocale(libc::LC_ALL_MASK, c.as_ptr(), std::ptr::null_mut());
                if loc.is_null() {
                    return false;
                }
                let set =
                    CStr::from_ptr(libc::nl_langinfo_l(libc::CODESET, loc)).to_bytes() == b"UTF-8";
                libc::freelocale(loc);
                set
            }
        };
        match name.split_once('.') {
            Some((_, "utf8")) if utf8(name) => Some(name.to_string()),
            Some(_) => None,
            None if utf8(name) => Some(name.to_string()),
            None => {
                let full = format!("{name}.utf8");
                utf8(&full).then_some(full)
            }
        }
    }

    /// ICU's own version, as `u_getVersion` gives it.
    pub fn icu_version() -> Option<String> {
        let mut info = [0u8; 4];
        let mut text = [0 as c_char; U_MAX_VERSION_STRING_LENGTH];
        // SAFETY: both buffers hold what ICU writes.
        unsafe {
            u_getVersion(info.as_mut_ptr());
            u_versionToString(info.as_ptr(), text.as_mut_ptr());
        }
        Some(
            unsafe { CStr::from_ptr(text.as_ptr()) }
                .to_string_lossy()
                .into_owned(),
        )
    }
}

pub use library::{get, icu_locales, icu_version, language_tag, libc_locale, open, Collator};

#[cfg(all(test, feature = "collations"))]
mod tests {
    use super::*;

    fn strings() -> Vec<String> {
        let mut out: Vec<String> = [
            "", " a", "a", "A", "b", "B", "z", "Z", "á", "Á", "a\u{301}", "ä", "ß", "ss", "SS",
            "é", "e", "ø", "\u{fb01}", "ΣΑΣ", "a b", "ab", "a-b", "a_b", "Ab", "!", "1", "10", "2",
            "\u{663}", "α", "Ω", "д", "Д", "א", "中", "あ", "\u{130}", "\u{1c6}", "😀",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
        let pool: Vec<char> = "aAbBeEéÉßsS İıi-_ 1٣ä\u{301}".chars().collect();
        for _ in 0..2000 {
            let mut s = String::new();
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            for k in 0..(x % 5) {
                s.push(pool[((x >> (8 * k + 3)) % pool.len() as u64) as usize]);
            }
            out.push(s);
        }
        out
    }

    #[test]
    fn an_icu_collation_s_keys_sort_as_it_compares() {
        let specs = [
            Spec::icu("und", None, true),
            Spec::icu("en-US", None, true),
            Spec::icu("und-u-ks-level2", None, false),
            Spec::icu("und", Some("&a < z"), true),
        ];
        let s = strings();
        for spec in &specs {
            let (_, c) = open(spec, "test").unwrap();
            for a in &s {
                for b in s.iter().step_by(7) {
                    assert_eq!(
                        c.key(a.as_bytes()).cmp(&c.key(b.as_bytes())),
                        c.cmp(a.as_bytes(), b.as_bytes()),
                        "{spec:?}: {a:?} against {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn glibc_s_keys_sort_as_strcoll_l_compares_the_forty_strings_and_are_checked_against_it() {
        let (_, c) = open(&Spec::libc("en_US.utf8", "en_US.utf8"), "test").unwrap();
        let s = strings();
        for a in &s[..40] {
            for b in &s[..40] {
                assert!(c.agrees(a.as_bytes(), b.as_bytes()), "{a:?} against {b:?}");
            }
        }
        let wrong = s
            .iter()
            .flat_map(|a| s.iter().step_by(3).map(move |b| (a, b)))
            .filter(|(a, b)| !c.agrees(a.as_bytes(), b.as_bytes()))
            .count();
        assert!(wrong < s.len() * s.len() / 3 / 100, "{wrong}");
    }

    #[test]
    fn a_deterministic_collation_breaks_its_ties_by_the_bytes_and_a_nondeterministic_one_not() {
        let (_, det) = open(&Spec::icu("und-u-ks-level2", None, true), "t").unwrap();
        let (_, nd) = open(&Spec::icu("und-u-ks-level2", None, false), "t").unwrap();
        assert_eq!(det.cmp(b"a", b"A"), Ordering::Greater);
        assert_eq!(nd.cmp(b"a", b"A"), Ordering::Equal);
        assert_eq!(nd.key(b"a"), nd.key(b"A"));
        assert_ne!(det.key(b"a"), det.key(b"A"));
        let (_, rules) = open(&Spec::icu("und", Some("&a < z"), true), "t").unwrap();
        assert_eq!(rules.cmp(b"z", b"b"), Ordering::Less);
    }

    #[test]
    fn case_is_mapped_by_the_collation_s_locale() {
        let (_, tr) = open(&Spec::icu("tr", None, true), "t").unwrap();
        let (_, und) = open(&Spec::icu("und", None, true), "t").unwrap();
        assert_eq!(tr.case("ijssel", true).unwrap(), "İJSSEL");
        assert_eq!(und.case("straße", true).unwrap(), "STRASSE");
        assert_eq!(und.case("ΣΑΣ ΟΔΟΣ", false).unwrap(), "σας οδος");
        let (_, us) = open(&Spec::libc("en_US.utf8", "en_US.utf8"), "t").unwrap();
        assert_eq!(us.case("straße", true).unwrap(), "STRAßE");
    }

    #[test]
    fn names_are_found_as_initdb_imports_them() {
        assert_eq!(language_tag("en_US.utf8").unwrap(), "en-US");
        assert!(icu_locales().iter().any(|t| t == "und"));
        assert!(icu_locales().iter().any(|t| t == "en-US"));
        assert_eq!(libc_locale("en_US").as_deref(), Some("en_US.utf8"));
        assert_eq!(libc_locale("en_US.utf8").as_deref(), Some("en_US.utf8"));
        assert_eq!(libc_locale("en_US.UTF-8"), None);
        let (_, c) = open(&Spec::libc("en_US.utf8", "en_US.utf8"), "t").unwrap();
        assert!(c.version().starts_with("2."));
        assert!(icu_version().unwrap().contains('.'));
    }
}
