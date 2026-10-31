//! A pattern's fixed prefix, as PostgreSQL 19's planner reads it out of LIKE, ILIKE, SIMILAR TO and
//! the regular expressions (`like_support.c`, `pattern_fixed_prefix`): the characters every string
//! the pattern matches begins with, or the one string it matches; and the range of a column's keys,
//! or the one key, that the question path narrows its read to by it (`match_pattern_prefix`).

use super::question::TextCond;
use crate::catalog::{Column, ColumnType};
use crate::Result;
use std::ops::Bound::{Excluded, Included, Unbounded};

/// What a pattern's analysis gives (`Pattern_Prefix_Status` and the prefix): no fixed prefix,
/// one every match begins with, or the one string it matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Fixed {
    None,
    Partial(Vec<u8>),
    Exact(Vec<u8>),
}

impl Fixed {
    fn of(prefix: Vec<u8>, whole: bool) -> Fixed {
        if whole {
            Fixed::Exact(prefix)
        } else if prefix.is_empty() {
            Fixed::None
        } else {
            Fixed::Partial(prefix)
        }
    }
}

/// `like_fixed_prefix` over a bytea's pattern: its bytes up to its first `%` or `_`, a backslash
/// taking the byte after it as itself; Exact where it reaches the pattern's end.
pub(super) fn like_fixed_prefix(pattern: &[u8]) -> Fixed {
    let escape = Some(b'\\');
    let mut prefix = Vec::new();
    let mut pos = 0;
    while pos < pattern.len() {
        let b = pattern[pos];
        if b == b'%' || b == b'_' {
            return Fixed::of(prefix, false);
        }
        if Some(b) == escape {
            pos += 1;
            if pos >= pattern.len() {
                return Fixed::of(prefix, false);
            }
        }
        prefix.push(pattern[pos]);
        pos += 1;
    }
    Fixed::Exact(prefix)
}

/// `like_fixed_prefix` over a text's pattern, its ESCAPE written in as `like_escape` writes it: its
/// characters up to its first `%` or `_`, each `escape` taking the one after it as itself; and as
/// `like_fixed_prefix_ci` reads ILIKE's, stopping too at a character of more than one case, which
/// `cased` says.
pub(super) fn like_fixed_prefix_chars(
    pattern: &str,
    escape: Option<char>,
    cased: impl Fn(char) -> bool,
) -> Fixed {
    let chars: Vec<char> = pattern.chars().collect();
    let mut prefix = String::new();
    let mut pos = 0;
    while pos < chars.len() {
        let c = chars[pos];
        if c == '%' || c == '_' {
            return Fixed::of(prefix.into_bytes(), false);
        }
        if Some(c) == escape {
            pos += 1;
            if pos >= chars.len() {
                return Fixed::of(prefix.into_bytes(), false);
            }
        }
        if cased(chars[pos]) {
            return Fixed::of(prefix.into_bytes(), false);
        }
        prefix.push(chars[pos]);
        pos += 1;
    }
    Fixed::Exact(prefix.into_bytes())
}

/// `regex_fixed_prefix`: the prefix the compiled expression gives (`regexp_fixed_prefix`).
pub(super) fn regex_fixed_prefix(
    pattern: &str,
    icase: bool,
    ctype: crate::regex::Ctype,
) -> Result<Fixed> {
    Ok(
        match crate::regexp::regexp_fixed_prefix(pattern, icase, ctype)? {
            Some((p, whole)) => Fixed::of(p.into_bytes(), whole),
            None => Fixed::None,
        },
    )
}

/// The least text greater than every text that begins with `p`: its last character the next
/// there is, a character with none taken off first; none where every character has none.
fn after_text(p: &[u8]) -> Option<Vec<u8>> {
    let mut chars: Vec<char> = String::from_utf8_lossy(p).chars().collect();
    while let Some(c) = chars.pop() {
        if let Some(n) = (c as u32 + 1..=char::MAX as u32).find_map(char::from_u32) {
            chars.push(n);
            return Some(chars.into_iter().collect::<String>().into_bytes());
        }
    }
    None
}

/// The least bytes greater than all that begin with `p`: its last byte one greater, a last byte
/// 0xff taken off first; none where every byte is 0xff.
fn after_bytes(p: &[u8]) -> Option<Vec<u8>> {
    let mut out = p.to_vec();
    while let Some(b) = out.pop() {
        if b < 0xff {
            out.push(b + 1);
            return Some(out);
        }
    }
    None
}

/// The condition a fixed prefix narrows a read on `column` to, as `match_pattern_prefix` reads one
/// for an index of the column's own collation: an exact pattern its one key, under any collation;
/// a prefix the run of keys that begin with it, only where the column's keys are its bytes. A
/// bpchar's prefix is read without its trailing blanks, as its keys are.
pub(super) fn narrowing(c: usize, column: &Column, fixed: Fixed) -> Result<Option<TextCond>> {
    let trimmed = |p: Vec<u8>| match column.ty {
        ColumnType::Bpchar(_) => crate::strings::bpchar_key(&p).to_vec(),
        _ => p,
    };
    Ok(match fixed {
        Fixed::None => None,
        Fixed::Exact(p) => {
            let p = trimmed(p);
            let key = match column.orderer()? {
                Some(k) => k.key(&p),
                None => p,
            };
            Some(TextCond::new(c, Included(key.clone()), Included(key)))
        }
        Fixed::Partial(_) if column.collated() => None,
        Fixed::Partial(p) => {
            let p = trimmed(p);
            let after = if column.ty.keyed_by_bytes() {
                after_bytes(&p)
            } else {
                after_text(&p)
            };
            Some(TextCond::new(
                c,
                Included(p),
                after.map_or(Unbounded, Excluded),
            ))
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_like_pattern_s_prefix_ends_at_its_first_wildcard_an_escape_taking_the_next_as_itself() {
        let like = |p: &[u8]| like_fixed_prefix(p);
        assert_eq!(like(b"abc%"), Fixed::Partial(b"abc".to_vec()));
        assert_eq!(like(b"ab_c"), Fixed::Partial(b"ab".to_vec()));
        assert_eq!(like(b"abc"), Fixed::Exact(b"abc".to_vec()));
        assert_eq!(like(b""), Fixed::Exact(Vec::new()));
        assert_eq!(like(b"%abc"), Fixed::None);
        assert_eq!(like(b"a\\%b%"), Fixed::Partial(b"a%b".to_vec()));
        assert_eq!(like(b"ab\\"), Fixed::Partial(b"ab".to_vec()));
        assert_eq!(like(b"\xff\x00%"), Fixed::Partial(b"\xff\x00".to_vec()));
        let text = |p: &str, e: Option<char>| like_fixed_prefix_chars(p, e, |_| false);
        assert_eq!(text("a#%b%", Some('#')), Fixed::Partial(b"a%b".to_vec()));
        assert_eq!(text("a\\b%", None), Fixed::Partial(b"a\\b".to_vec()));
        assert_eq!(text("aé%b%", Some('é')), Fixed::Partial(b"a%b".to_vec()));
        assert_eq!(text("aé%b", Some('é')), Fixed::Exact(b"a%b".to_vec()));
        assert_eq!(
            text("é_", Some('\\')),
            Fixed::Partial("é".as_bytes().to_vec())
        );
        let ci = |p: &str| like_fixed_prefix_chars(p, Some('\\'), |c| c.is_alphabetic());
        assert_eq!(ci("12-ab%"), Fixed::Partial(b"12-".to_vec()));
        assert_eq!(ci("12-3"), Fixed::Exact(b"12-3".to_vec()));
        assert_eq!(ci("ab%"), Fixed::None);
    }

    #[test]
    fn the_key_after_a_prefix_is_the_least_past_every_key_that_begins_with_it() {
        assert_eq!(after_text(b"abc"), Some(b"abd".to_vec()));
        assert_eq!(
            after_text("a\u{d7ff}".as_bytes()),
            Some("a\u{e000}".as_bytes().to_vec())
        );
        assert_eq!(after_text("ab\u{10ffff}".as_bytes()), Some(b"ac".to_vec()));
        assert_eq!(after_text("\u{10ffff}".as_bytes()), None);
        assert_eq!(after_bytes(b"a\xff\xff"), Some(b"b".to_vec()));
        assert_eq!(after_bytes(b"\xff"), None);
        // Each text that begins with the prefix lies before the key after it, and that key does
        // not begin with it.
        for p in ["ab", "a\u{7f}", "é", "a\u{ffff}"] {
            let after = after_text(p.as_bytes()).unwrap();
            for s in [
                p.to_string(),
                format!("{p}\u{0}"),
                format!("{p}\u{10ffff}\u{10ffff}"),
            ] {
                assert!(s.as_bytes() < after.as_slice(), "{p:?} {s:?}");
            }
            assert!(!String::from_utf8(after).unwrap().starts_with(p));
        }
    }
}
