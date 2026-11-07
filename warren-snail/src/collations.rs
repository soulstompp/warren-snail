//! Text under a collation that orders it otherwise than by its bytes: an ICU collator, or a glibc
//! locale, each called as PostgreSQL 19 calls it (`pg_locale_icu.c`, `pg_locale_libc.c`). Built
//! only with the feature `collations`, which links the machine's ICU; without it such a collation is
//! named and refused wherever its order or its case is asked.

use crate::Error;
#[cfg(not(feature = "collations"))]
use crate::Result;
#[cfg(not(feature = "collations"))]
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

/// A class of characters, as a regular expression asks a collation's library for it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CharClass {
    Digit,
    Alpha,
    Alnum,
    Upper,
    Lower,
    Graph,
    Print,
    Punct,
    Space,
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
        pub fn title(&self, _s: &str) -> Result<String> {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn fold(&self, _s: &str) -> Result<String> {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn version(&self) -> &str {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn ctype_is_c(&self) -> bool {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn is_class(&self, _class: CharClass, _c: u32) -> bool {
            unreachable!("no collator opens without the feature collations")
        }
        pub fn char_case(&self, _c: u32, _upper: bool) -> u32 {
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
use crate::sys::collate as library;

pub use library::{get, icu_locales, icu_version, language_tag, libc_locale, open, Collator};

#[cfg(all(test, feature = "collations"))]
mod tests {
    use super::*;
    use std::cmp::Ordering;

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
