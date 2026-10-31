//! The SQL functions of regular expressions, as PostgreSQL 19's regexp.c gives them: `~` and its
//! kin, substring by a pattern, the `regexp_` functions and SIMILAR TO's translation, each over a
//! text and a pattern compiled under the collation's classes.

use crate::regex::Ctype;
use crate::Result;

/// The options a `regexp_` function's flags argument gives (`pg_re_flags`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReFlags {
    pub cflags: i32,
    pub glob: bool,
}

/// `parse_re_flags`: the options of a flags argument, or the defaults where there is none.
pub fn parse_re_flags(_opts: Option<&str>) -> Result<ReFlags> {
    todo!("regexp.c")
}

/// `~` and, where `icase`, `~*` (`textregexeq`, `texticregexeq`); `!~` and `!~*` are their
/// negations.
pub fn textregexeq(_s: &str, _p: &str, _icase: bool, _ctype: Ctype) -> Result<bool> {
    todo!("regexp.c")
}

/// `substring(s from p)` (`textregexsubstr`).
pub fn textregexsubstr(_s: &str, _p: &str, _ctype: Ctype) -> Result<Option<String>> {
    todo!("regexp.c")
}

/// `regexp_replace(s, p, r)` (`textregexreplace_noopt`).
pub fn textregexreplace_noopt(_s: &str, _p: &str, _r: &str, _ctype: Ctype) -> Result<String> {
    todo!("regexp.c")
}

/// `regexp_replace(s, p, r, flags text)` (`textregexreplace`).
pub fn textregexreplace(_s: &str, _p: &str, _r: &str, _opt: &str, _ctype: Ctype) -> Result<String> {
    todo!("regexp.c")
}

/// `regexp_replace(s, p, r, start [, n [, flags]])` (`textregexreplace_extended`): `n` and `flags`
/// None where the form has no such argument.
pub fn textregexreplace_extended(
    _s: &str,
    _p: &str,
    _r: &str,
    _start: i32,
    _n: Option<i32>,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<String> {
    todo!("regexp.c")
}

/// `similar_escape(pat, esc)`, which is not strict: None where `pat` is NULL; `esc` NULL for the
/// default escape.
pub fn similar_escape(_pat: Option<&str>, _esc: Option<&str>) -> Result<Option<String>> {
    todo!("regexp.c")
}

/// `similar_to_escape(pat [, esc])`: `esc` None for the form of one argument.
pub fn similar_to_escape(_pat: &str, _esc: Option<&str>) -> Result<String> {
    todo!("regexp.c")
}

/// `regexp_count(s, p [, start [, flags]])`.
pub fn regexp_count(
    _s: &str,
    _p: &str,
    _start: Option<i32>,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<i32> {
    todo!("regexp.c")
}

/// `regexp_instr(s, p [, start [, n [, endoption [, flags [, subexpr]]]]])`.
#[allow(clippy::too_many_arguments)]
pub fn regexp_instr(
    _s: &str,
    _p: &str,
    _start: Option<i32>,
    _n: Option<i32>,
    _endoption: Option<i32>,
    _flags: Option<&str>,
    _subexpr: Option<i32>,
    _ctype: Ctype,
) -> Result<i32> {
    todo!("regexp.c")
}

/// `regexp_like(s, p [, flags])`.
pub fn regexp_like(_s: &str, _p: &str, _flags: Option<&str>, _ctype: Ctype) -> Result<bool> {
    todo!("regexp.c")
}

/// `regexp_match(s, p [, flags])`: the array's elements, None where it does not match.
pub fn regexp_match(
    _s: &str,
    _p: &str,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<Option<Vec<Option<String>>>> {
    todo!("regexp.c")
}

/// `regexp_matches(s, p [, flags])`: its rows, each an array's elements.
pub fn regexp_matches(
    _s: &str,
    _p: &str,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<Vec<Vec<Option<String>>>> {
    todo!("regexp.c")
}

/// `regexp_split_to_table(s, p [, flags])`: its rows.
pub fn regexp_split_to_table(
    _s: &str,
    _p: &str,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<Vec<String>> {
    todo!("regexp.c")
}

/// `regexp_split_to_array(s, p [, flags])`: the array's elements.
pub fn regexp_split_to_array(
    _s: &str,
    _p: &str,
    _flags: Option<&str>,
    _ctype: Ctype,
) -> Result<Vec<String>> {
    todo!("regexp.c")
}

/// `regexp_substr(s, p [, start [, n [, flags [, subexpr]]]])`.
pub fn regexp_substr(
    _s: &str,
    _p: &str,
    _start: Option<i32>,
    _n: Option<i32>,
    _flags: Option<&str>,
    _subexpr: Option<i32>,
    _ctype: Ctype,
) -> Result<Option<String>> {
    todo!("regexp.c")
}
