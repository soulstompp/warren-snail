//! The SQL functions of regular expressions, as PostgreSQL 19's regexp.c gives them: `~` and its
//! kin, substring by a pattern, the `regexp_` functions and SIMILAR TO's translation, each over a
//! text and a pattern compiled under the collation's classes.

use std::cell::RefCell;
use std::rc::Rc;

use crate::regex::guts::{
    REG_ADVANCED, REG_EXPANDED, REG_EXTENDED, REG_ICASE, REG_NEWLINE, REG_NLANCH, REG_NLSTOP,
    REG_NOSUB, REG_QUOTE,
};
use crate::regex::{Chr, Ctype, RegMatch, Regex};
use crate::{Error, Result};

/// The options a `regexp_` function's flags argument gives (`pg_re_flags`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReFlags {
    pub cflags: i32,
    pub glob: bool,
}

/// How many compiled patterns are kept (`MAX_CACHED_RES`).
const MAX_CACHED_RES: usize = 32;
/// The greatest allocation PostgreSQL makes (`MaxAllocSize`).
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

/// One compiled pattern kept, by its text, its flags and its collation's classes
/// (`cached_re_str`).
struct CachedRe {
    pat: String,
    cflags: i32,
    ctype: Ctype,
    re: Rc<Regex>,
}

thread_local! {
    /// The compiled patterns kept, the most recently used first (`re_array`).
    static RE_ARRAY: RefCell<Vec<CachedRe>> = const { RefCell::new(Vec::new()) };
}

/// A string's characters, as `pg_mb2wchar_with_len` makes them.
fn wide(s: &str) -> Vec<Chr> {
    s.chars().map(Chr::from).collect()
}

/// The characters from `so` to `eo` of a string held as its characters, as `text_substr` gives
/// them for a start of `so + 1` and a length of `eo - so`.
fn substr(chars: &[Chr], so: i64, eo: i64) -> String {
    (chars[so as usize..eo as usize].iter())
        .filter_map(|&c| char::from_u32(c))
        .collect()
}

/// `RE_compile_and_cache`: a pattern compiled under the flags and the collation's classes, taken
/// from the patterns kept where it is among them (and moved to their front), else compiled and
/// kept in front, the least recently used dropped past `MAX_CACHED_RES`.
fn re_compile_and_cache(pat: &str, cflags: i32, ctype: Ctype) -> Result<Rc<Regex>> {
    let kept = RE_ARRAY.with(|a| {
        let mut a = a.borrow_mut();
        let i = (a.iter()).position(|e| e.cflags == cflags && e.ctype == ctype && e.pat == pat)?;
        let e = a.remove(i);
        let re = Rc::clone(&e.re);
        a.insert(0, e);
        Some(re)
    });
    if let Some(re) = kept {
        return Ok(re);
    }

    let re = Regex::compile(&wide(pat), cflags, ctype)
        .map_err(|e| Error::new(format!("invalid regular expression: {}", e.message())))?;
    let re = Rc::new(re);
    RE_ARRAY.with(|a| {
        let mut a = a.borrow_mut();
        a.truncate(MAX_CACHED_RES - 1);
        a.insert(
            0,
            CachedRe {
                pat: pat.to_string(),
                cflags,
                ctype,
                re: Rc::clone(&re),
            },
        );
    });
    Ok(re)
}

/// `RE_wchar_execute`: the string, as its characters, searched from `start_search`; the first
/// `nmatch` matches where it matches, None where it does not.
fn re_wchar_execute(
    re: &Regex,
    data: &[Chr],
    start_search: usize,
    nmatch: usize,
) -> Result<Option<Vec<RegMatch>>> {
    re.exec(data, start_search, nmatch, 0)
        .map_err(|e| Error::new(format!("regular expression failed: {}", e.message())))
}

/// `RE_execute`: the string searched from its start.
fn re_execute(re: &Regex, dat: &str, nmatch: usize) -> Result<Option<Vec<RegMatch>>> {
    re_wchar_execute(re, &wide(dat), 0, nmatch)
}

/// `RE_compile_and_execute`: the pattern compiled, without the captures' matches where fewer than
/// two matches are wanted, and the string searched.
fn re_compile_and_execute(
    pat: &str,
    dat: &str,
    mut cflags: i32,
    ctype: Ctype,
    nmatch: usize,
) -> Result<Option<Vec<RegMatch>>> {
    if nmatch < 2 {
        cflags |= REG_NOSUB;
    }
    let re = re_compile_and_cache(pat, cflags, ctype)?;
    re_execute(&re, dat, nmatch)
}

/// The error of an option no regular expression takes: the option's character, as PostgreSQL
/// prints it.
fn invalid_option(rest: &str) -> Error {
    let c: String = rest.chars().take(1).collect();
    Error::new(format!("invalid regular expression option: \"{c}\""))
}

/// `parse_re_flags`: the options of a flags argument, or the defaults where there is none. Every
/// option any of the functions takes is accepted; each function refuses those it does not take.
pub fn parse_re_flags(opts: Option<&str>) -> Result<ReFlags> {
    let mut flags = ReFlags {
        cflags: REG_ADVANCED,
        glob: false,
    };
    let Some(opts) = opts else {
        return Ok(flags);
    };
    for (i, b) in opts.bytes().enumerate() {
        match b {
            b'g' => flags.glob = true,
            // Basic expressions.
            b'b' => flags.cflags &= !(REG_ADVANCED | REG_EXTENDED | REG_QUOTE),
            // Case sensitive.
            b'c' => flags.cflags &= !REG_ICASE,
            // Plain extended expressions.
            b'e' => {
                flags.cflags |= REG_EXTENDED;
                flags.cflags &= !(REG_ADVANCED | REG_QUOTE);
            }
            // Case insensitive.
            b'i' => flags.cflags |= REG_ICASE,
            // A newline ends a line for ^, $, . and [^.
            b'm' | b'n' => flags.cflags |= REG_NEWLINE,
            // A newline ends a line for . and [^.
            b'p' => {
                flags.cflags |= REG_NLSTOP;
                flags.cflags &= !REG_NLANCH;
            }
            // A literal string.
            b'q' => {
                flags.cflags |= REG_QUOTE;
                flags.cflags &= !(REG_ADVANCED | REG_EXTENDED);
            }
            // A newline is an ordinary character.
            b's' => flags.cflags &= !REG_NEWLINE,
            // Tight syntax.
            b't' => flags.cflags &= !REG_EXPANDED,
            // A newline ends a line for ^ and $.
            b'w' => {
                flags.cflags &= !REG_NLSTOP;
                flags.cflags |= REG_NLANCH;
            }
            // Expanded syntax.
            b'x' => flags.cflags |= REG_EXPANDED,
            _ => return Err(invalid_option(&opts[i..])),
        }
    }
    Ok(flags)
}

/// The error of a function that does not take the `g` option.
fn no_global(function: &str) -> Error {
    Error::new(format!("{function} does not support the \"global\" option"))
}

/// The error of an integer argument out of its range.
fn invalid_value(parameter: &str, value: i32) -> Error {
    Error::new(format!(
        "invalid value for parameter \"{parameter}\": {value}"
    ))
}

/// `~` and, where `icase`, `~*` (`textregexeq`, `texticregexeq`); `!~` and `!~*` are their
/// negations.
pub fn textregexeq(s: &str, p: &str, icase: bool, ctype: Ctype) -> Result<bool> {
    let cflags = REG_ADVANCED | if icase { REG_ICASE } else { 0 };
    Ok(re_compile_and_execute(p, s, cflags, ctype, 0)?.is_some())
}

/// `substring(s from p)` (`textregexsubstr`): what the first capture matched where the pattern
/// captures, else what the whole pattern matched; None where it does not match, or where the
/// capture matched nothing.
pub fn textregexsubstr(s: &str, p: &str, ctype: Ctype) -> Result<Option<String>> {
    let re = re_compile_and_cache(p, REG_ADVANCED, ctype)?;
    let data = wide(s);
    let Some(pmatch) = re_wchar_execute(&re, &data, 0, 2)? else {
        return Ok(None);
    };
    let m = if re.nsub() > 0 { pmatch[1] } else { pmatch[0] };
    if m.rm_so < 0 || m.rm_eo < 0 {
        return Ok(None);
    }
    Ok(Some(substr(&data, m.rm_so, m.rm_eo)))
}

/// `regexp_replace(s, p, r)` (`textregexreplace_noopt`): the first match replaced.
pub fn textregexreplace_noopt(s: &str, p: &str, r: &str, ctype: Ctype) -> Result<String> {
    replace_text_regexp(s, p, r, REG_ADVANCED, ctype, 0, 1)
}

/// `regexp_replace(s, p, r, flags text)` (`textregexreplace`): the first match replaced, or each
/// under `g`. Flags that start with a digit are refused as an option, as PostgreSQL refuses them
/// for a start meant as an integer.
pub fn textregexreplace(s: &str, p: &str, r: &str, opt: &str, ctype: Ctype) -> Result<String> {
    if opt.as_bytes().first().is_some_and(u8::is_ascii_digit) {
        return Err(invalid_option(opt));
    }
    let flags = parse_re_flags(Some(opt))?;
    replace_text_regexp(
        s,
        p,
        r,
        flags.cflags,
        ctype,
        0,
        if flags.glob { 0 } else { 1 },
    )
}

/// `regexp_replace(s, p, r, start [, n [, flags]])` (`textregexreplace_extended`): `n` and `flags`
/// None where the form has no such argument. The search starts at character `start`; the `n`th
/// match is replaced, or each where `n` is 0; without `n`, the first, or each under `g`.
#[allow(clippy::too_many_arguments)]
pub fn textregexreplace_extended(
    s: &str,
    p: &str,
    r: &str,
    start: i32,
    n: Option<i32>,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<String> {
    if start <= 0 {
        return Err(invalid_value("start", start));
    }
    if let Some(n) = n {
        if n < 0 {
            return Err(invalid_value("n", n));
        }
    }
    let re_flags = parse_re_flags(flags)?;
    let n = match n {
        Some(n) => n,
        None if re_flags.glob => 0,
        None => 1,
    };
    replace_text_regexp(s, p, r, re_flags.cflags, ctype, start as usize - 1, n)
}

/// `check_replace_text_has_escape`: 0 where the replacement has no backslash to process, 1 where
/// it has some but no reference to a capture (`\1` to `\9`), 2 where it has one.
fn check_replace_text_has_escape(replace: &[u8]) -> i32 {
    let mut result = 0;
    let mut p = 0;
    while p < replace.len() {
        let Some(at) = replace[p..].iter().position(|&b| b == b'\\') else {
            break;
        };
        p += at + 1;
        // A backslash at the end needs no processing.
        if p < replace.len() {
            if (b'1'..=b'9').contains(&replace[p]) {
                return 2;
            }
            result = 1;
            p += 1;
        }
    }
    result
}

/// `appendStringInfoRegexpSubstr`: the replacement appended, `\1` to `\9` replaced by what each
/// capture matched (nothing where it matched nothing), `\&` by the whole match, `\\` by one
/// backslash, and any other backslash kept as it is.
fn append_regexp_substr(buf: &mut Vec<u8>, replace: &[u8], pmatch: &[RegMatch], data: &[Chr]) {
    let mut p = 0;
    while p < replace.len() {
        let chunk_start = p;
        p = (replace[p..].iter())
            .position(|&b| b == b'\\')
            .map_or(replace.len(), |at| p + at);

        // The text scanned over.
        buf.extend_from_slice(&replace[chunk_start..p]);

        if p >= replace.len() {
            break;
        }
        p += 1;

        if p >= replace.len() {
            // A backslash at the very end is kept.
            buf.push(b'\\');
            break;
        }

        let m = match replace[p] {
            c @ b'1'..=b'9' => {
                p += 1;
                pmatch
                    .get((c - b'0') as usize)
                    .copied()
                    .unwrap_or(RegMatch::UNSET)
            }
            b'&' => {
                p += 1;
                pmatch[0]
            }
            b'\\' => {
                buf.push(b'\\');
                p += 1;
                continue;
            }
            _ => {
                buf.push(b'\\');
                continue;
            }
        };

        if m.rm_so >= 0 && m.rm_eo >= 0 {
            buf.extend_from_slice(substr(data, m.rm_so, m.rm_eo).as_bytes());
        }
    }
}

/// `replace_text_regexp`: the matches of the pattern in `src` replaced by `replace`, the search
/// starting at character `search_start`: every match where `n` is 0, else only the `n`th. An
/// empty match moves the search on by one character.
fn replace_text_regexp(
    src: &str,
    pattern: &str,
    replace: &str,
    mut cflags: i32,
    ctype: Ctype,
    mut search_start: usize,
    n: i32,
) -> Result<String> {
    let data = wide(src);
    let data_len = data.len();
    let replace = replace.as_bytes();
    let mut nmatch = 10;
    let mut nmatches = 0;

    // Without references to captures, the captures' matches are not needed.
    let escape_status = check_replace_text_has_escape(replace);
    if escape_status < 2 {
        cflags |= REG_NOSUB;
        nmatch = 1;
    }

    let re = re_compile_and_cache(pattern, cflags, ctype)?;

    let mut buf: Vec<u8> = Vec::new();
    // The characters before `data_pos` are in `buf` already.
    let mut data_pos = 0;

    while search_start <= data_len {
        let Some(pmatch) = re_wchar_execute(&re, &data, search_start, nmatch)? else {
            break;
        };
        let (so, eo) = (pmatch[0].rm_so as usize, pmatch[0].rm_eo as usize);

        // Whether this is the match to replace.
        nmatches += 1;
        if n > 0 && nmatches != n {
            // Not this one: its text is copied later, as if it had not matched.
            search_start = eo;
            if so == eo {
                search_start += 1;
            }
            continue;
        }

        // The text before the match.
        if so > data_pos {
            buf.extend_from_slice(substr(&data, data_pos as i64, so as i64).as_bytes());
        }

        // The replacement.
        if escape_status > 0 {
            append_regexp_substr(&mut buf, replace, &pmatch, &data);
        } else {
            buf.extend_from_slice(replace);
        }

        data_pos = eo;

        if n > 0 {
            break;
        }

        search_start = data_pos;
        if so == eo {
            search_start += 1;
        }
    }

    // The text after the last match.
    if data_pos < data_len {
        buf.extend_from_slice(substr(&data, data_pos as i64, data_len as i64).as_bytes());
    }

    String::from_utf8(buf).map_err(|_| Error::new("invalid byte sequence for encoding \"UTF8\""))
}

/// `similar_escape_internal`: a SIMILAR TO pattern rewritten as a regular expression matching the
/// whole string, `^(?:` ... `)$`: `%` as `.*`, `_` as `.`, a parenthesis as one that does not
/// capture, the regular expressions' own specials escaped; the escape character (a backslash
/// where `esc` is None, none where it is empty) escaping the character after it; and where
/// escape-double-quotes divide the pattern in up to three parts, the first matched as briefly as
/// it can be and the second captured.
fn similar_escape_internal(pat: &str, esc: Option<&str>) -> Result<String> {
    let p = pat.as_bytes();
    let e: Option<&[u8]> = match esc {
        None => Some(b"\\"),
        Some("") => None,
        Some(esc) => {
            if esc.chars().count() > 1 {
                return Err(Error::new("invalid escape string"));
            }
            Some(esc.as_bytes())
        }
    };
    let elen = e.map_or(0, <[u8]>::len);

    let mut afterescape = false;
    let mut nquotes = 0;
    let mut bracket_depth = 0;
    let mut charclass_pos = 0;

    let mut r: Vec<u8> = Vec::with_capacity(23 + 3 * p.len());
    r.extend_from_slice(b"^(?:");

    let mut i = 0;
    while i < p.len() {
        let pchar = p[i];

        // Where both the escape and the character are of more than one byte, the character is
        // taken whole; else the pattern is read a byte at a time, as no byte of a character of
        // more than one byte is a character of one.
        if elen > 1 {
            let mblen = pat[i..].chars().next().map_or(1, char::len_utf8);
            if mblen > 1 {
                let ch = &p[i..i + mblen];
                if afterescape {
                    r.push(b'\\');
                    r.extend_from_slice(ch);
                    afterescape = false;
                } else if e.is_some_and(|e| e == ch) {
                    // The escape: not sent to the output.
                    afterescape = true;
                } else {
                    r.extend_from_slice(ch);
                }
                i += mblen;
                continue;
            }
        }

        if afterescape {
            if pchar == b'"' && bracket_depth < 1 {
                // An escape-double-quote: the separator of its part.
                match nquotes {
                    0 => r.extend_from_slice(b"){1,1}?("),
                    1 => r.extend_from_slice(b"){1,1}(?:"),
                    _ => {
                        return Err(Error::new(
                            "SQL regular expression may not contain more than two \
                             escape-double-quote separators",
                        ))
                    }
                }
                nquotes += 1;
            } else {
                // Any character may be escaped.
                r.push(b'\\');
                r.push(pchar);
                // An escaped character in a class is past the class's start.
                charclass_pos = 3;
            }
            afterescape = false;
        } else if e.is_some_and(|e| pchar == e[0]) {
            // The escape: not sent to the output.
            afterescape = true;
        } else if bracket_depth > 0 {
            // Inside a class: a backslash that is not the escape is a member, doubled.
            if pchar == b'\\' {
                r.push(b'\\');
            }
            r.push(pchar);

            if pchar == b']' && charclass_pos > 2 {
                // The end of a bracket pair.
                bracket_depth -= 1;
            } else if pchar == b'[' {
                // A nested bracket pair: a collating element, past the class's start.
                bracket_depth += 1;
                charclass_pos = 3;
            } else if pchar == b'^' {
                // A caret right after the opening bracket negates the class; a ']' after it
                // is still a member.
                charclass_pos += 1;
            } else {
                charclass_pos = 3;
            }
        } else if pchar == b'[' {
            // The start of a class.
            r.push(pchar);
            bracket_depth = 1;
            charclass_pos = 1;
        } else if pchar == b'%' {
            r.extend_from_slice(b".*");
        } else if pchar == b'_' {
            r.push(b'.');
        } else if pchar == b'(' {
            // A parenthesis that does not capture.
            r.extend_from_slice(b"(?:");
        } else if matches!(pchar, b'\\' | b'.' | b'^' | b'$') {
            r.push(b'\\');
            r.push(pchar);
        } else {
            r.push(pchar);
        }
        i += 1;
    }

    r.extend_from_slice(b")$");

    String::from_utf8(r).map_err(|_| Error::new("invalid byte sequence for encoding \"UTF8\""))
}

/// `similar_escape(pat, esc)`, which is not strict: None where `pat` is NULL; `esc` NULL for the
/// default escape.
pub fn similar_escape(pat: Option<&str>, esc: Option<&str>) -> Result<Option<String>> {
    let Some(pat) = pat else {
        return Ok(None);
    };
    similar_escape_internal(pat, esc).map(Some)
}

/// `similar_to_escape(pat [, esc])`: `esc` None for the form of one argument, whose escape is the
/// default.
pub fn similar_to_escape(pat: &str, esc: Option<&str>) -> Result<String> {
    similar_escape_internal(pat, esc)
}

/// What `setup_regexp_matches` finds (`regexp_matches_ctx`): the string's characters, and for
/// each match the start and end of each capture's match (of the whole match's where the
/// captures are not wanted), -1 and -1 for one that matched nothing; the string's length after
/// the last.
struct MatchCtx {
    wide_str: Vec<Chr>,
    nmatches: usize,
    npatterns: usize,
    match_locs: Vec<i64>,
}

impl MatchCtx {
    /// The text from `so` to `eo`, None where either is -1.
    fn text(&self, so: i64, eo: i64) -> Option<String> {
        if so < 0 || eo < 0 {
            return None;
        }
        Some(substr(&self.wide_str, so, eo))
    }
}

/// `setup_regexp_matches`: every match of the pattern in the string from character
/// `start_search` (only the first where the flags have no `g`), each next search starting at the
/// end of the last match, one character on where it was empty. `use_subpatterns`: the captures'
/// matches are kept, where the pattern has any. `ignore_degenerate`: empty matches at the string's
/// start, at its end, or right after another match are passed over.
fn setup_regexp_matches(
    orig: &str,
    pattern: &str,
    re_flags: ReFlags,
    mut start_search: usize,
    ctype: Ctype,
    mut use_subpatterns: bool,
    ignore_degenerate: bool,
) -> Result<MatchCtx> {
    let wide_str = wide(orig);
    let wide_len = wide_str.len();

    let mut cflags = re_flags.cflags;
    if !use_subpatterns {
        cflags |= REG_NOSUB;
    }
    let cpattern = re_compile_and_cache(pattern, cflags, ctype)?;

    let (npatterns, pmatch_len) = if use_subpatterns && cpattern.nsub() > 0 {
        (cpattern.nsub(), cpattern.nsub() + 1)
    } else {
        use_subpatterns = false;
        (1, 1)
    };

    // The room C's array of places has, grown as C grows it.
    let mut array_len: usize = if re_flags.glob { 255 } else { 31 };
    let mut match_locs: Vec<i64> = Vec::new();
    let mut nmatches = 0;

    let mut prev_match_end: i64 = 0;
    while let Some(pmatch) = re_wchar_execute(&cpattern, &wide_str, start_search, pmatch_len)? {
        if !ignore_degenerate
            || (pmatch[0].rm_so < wide_len as i64 && pmatch[0].rm_eo > prev_match_end)
        {
            while match_locs.len() + npatterns * 2 + 1 > array_len {
                array_len = 2 * array_len + 1;
                if array_len > MAX_ALLOC_SIZE / 4 {
                    return Err(Error::new("too many regular expression matches"));
                }
            }

            if use_subpatterns {
                for m in &pmatch[1..=npatterns] {
                    match_locs.push(m.rm_so);
                    match_locs.push(m.rm_eo);
                }
            } else {
                match_locs.push(pmatch[0].rm_so);
                match_locs.push(pmatch[0].rm_eo);
            }
            nmatches += 1;
        }
        prev_match_end = pmatch[0].rm_eo;

        if !re_flags.glob {
            break;
        }

        // The next search starts at the end of this match, one character on where it was
        // empty.
        start_search = prev_match_end as usize;
        if pmatch[0].rm_so == pmatch[0].rm_eo {
            start_search += 1;
        }
        if start_search > wide_len {
            break;
        }
    }

    // The end of the string, for the split.
    match_locs.push(wide_len as i64);

    Ok(MatchCtx {
        wide_str,
        nmatches,
        npatterns,
        match_locs,
    })
}

/// `build_regexp_match_result`: the `next_match`'th match's array, one element a capture (or the
/// whole match), NULL where it matched nothing.
fn build_regexp_match_result(ctx: &MatchCtx, next_match: usize) -> Vec<Option<String>> {
    let loc = next_match * ctx.npatterns * 2;
    (0..ctx.npatterns)
        .map(|i| ctx.text(ctx.match_locs[loc + 2 * i], ctx.match_locs[loc + 2 * i + 1]))
        .collect()
}

/// `build_regexp_split_result`: the text between the match before the `next_match`'th (the
/// string's start for the first) and that match (the string's end after the last).
fn build_regexp_split_result(ctx: &MatchCtx, next_match: usize) -> Result<String> {
    let startpos = if next_match > 0 {
        ctx.match_locs[next_match * 2 - 1]
    } else {
        0
    };
    if startpos < 0 {
        return Err(Error::new("invalid match ending position"));
    }
    let endpos = ctx.match_locs[next_match * 2];
    if endpos < startpos {
        return Err(Error::new("invalid match starting position"));
    }
    Ok(substr(&ctx.wide_str, startpos, endpos))
}

/// A `start` argument checked, as a character offset from the string's start.
fn start_offset(start: Option<i32>) -> Result<usize> {
    match start {
        Some(start) if start <= 0 => Err(invalid_value("start", start)),
        Some(start) => Ok(start as usize - 1),
        None => Ok(0),
    }
}

/// An `n` argument checked: positive, 1 where there is none.
fn nth(n: Option<i32>) -> Result<usize> {
    match n {
        Some(n) if n <= 0 => Err(invalid_value("n", n)),
        Some(n) => Ok(n as usize),
        None => Ok(1),
    }
}

/// A `subexpr` argument checked: not negative, 0 (the whole match) where there is none.
fn subexpression(subexpr: Option<i32>) -> Result<usize> {
    match subexpr {
        Some(s) if s < 0 => Err(invalid_value("subexpr", s)),
        Some(s) => Ok(s as usize),
        None => Ok(0),
    }
}

/// The flags of a function that finds every match but takes no `g`.
fn every_match(flags: Option<&str>, function: &str) -> Result<ReFlags> {
    let mut re_flags = parse_re_flags(flags)?;
    if re_flags.glob {
        return Err(no_global(function));
    }
    re_flags.glob = true;
    Ok(re_flags)
}

/// `regexp_count(s, p [, start [, flags]])`: how many times the pattern matches from character
/// `start` on.
pub fn regexp_count(
    s: &str,
    p: &str,
    start: Option<i32>,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<i32> {
    let start = start_offset(start)?;
    let re_flags = every_match(flags, "regexp_count()")?;
    let ctx = setup_regexp_matches(s, p, re_flags, start, ctype, false, false)?;
    Ok(ctx.nmatches as i32)
}

/// `regexp_instr(s, p [, start [, n [, endoption [, flags [, subexpr]]]]])`: where the `n`th
/// match from character `start` on (or its `subexpr`th capture's match) starts, or where it ends
/// where `endoption` is 1, counted from 1; 0 where there is no such match.
#[allow(clippy::too_many_arguments)]
pub fn regexp_instr(
    s: &str,
    p: &str,
    start: Option<i32>,
    n: Option<i32>,
    endoption: Option<i32>,
    flags: Option<&str>,
    subexpr: Option<i32>,
    ctype: Ctype,
) -> Result<i32> {
    let start = start_offset(start)?;
    let n = nth(n)?;
    let endoption = endoption.unwrap_or(0);
    if endoption != 0 && endoption != 1 {
        return Err(invalid_value("endoption", endoption));
    }
    let subexpr = subexpression(subexpr)?;
    let re_flags = every_match(flags, "regexp_instr()")?;

    let ctx = setup_regexp_matches(s, p, re_flags, start, ctype, subexpr > 0, false)?;

    if n > ctx.nmatches || subexpr > ctx.npatterns {
        return Ok(0);
    }
    let mut pos = (n - 1) * ctx.npatterns;
    if subexpr > 0 {
        pos += subexpr - 1;
    }
    pos *= 2;
    if endoption == 1 {
        pos += 1;
    }
    let loc = ctx.match_locs[pos];
    Ok(if loc >= 0 { loc as i32 + 1 } else { 0 })
}

/// `regexp_like(s, p [, flags])`: whether the pattern matches.
pub fn regexp_like(s: &str, p: &str, flags: Option<&str>, ctype: Ctype) -> Result<bool> {
    let re_flags = parse_re_flags(flags)?;
    if re_flags.glob {
        return Err(no_global("regexp_like()"));
    }
    Ok(re_compile_and_execute(p, s, re_flags.cflags, ctype, 0)?.is_some())
}

/// `regexp_match(s, p [, flags])`: the array's elements, None where it does not match.
pub fn regexp_match(
    s: &str,
    p: &str,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<Option<Vec<Option<String>>>> {
    let re_flags = parse_re_flags(flags)?;
    if re_flags.glob {
        return Err(no_global("regexp_match()"));
    }
    let ctx = setup_regexp_matches(s, p, re_flags, 0, ctype, true, false)?;
    if ctx.nmatches == 0 {
        return Ok(None);
    }
    Ok(Some(build_regexp_match_result(&ctx, 0)))
}

/// `regexp_matches(s, p [, flags])`: its rows, each an array's elements: the first match, or
/// under `g` every match.
pub fn regexp_matches(
    s: &str,
    p: &str,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<Vec<Vec<Option<String>>>> {
    let re_flags = parse_re_flags(flags)?;
    let ctx = setup_regexp_matches(s, p, re_flags, 0, ctype, true, false)?;
    Ok((0..ctx.nmatches)
        .map(|m| build_regexp_match_result(&ctx, m))
        .collect())
}

/// The pieces of a string between the matches of a pattern, the empty matches at its edges and
/// right after another match passed over.
fn regexp_split(
    s: &str,
    p: &str,
    flags: Option<&str>,
    ctype: Ctype,
    function: &str,
) -> Result<Vec<String>> {
    let re_flags = every_match(flags, function)?;
    let ctx = setup_regexp_matches(s, p, re_flags, 0, ctype, false, true)?;
    (0..=ctx.nmatches)
        .map(|m| build_regexp_split_result(&ctx, m))
        .collect()
}

/// `regexp_split_to_table(s, p [, flags])`: its rows.
pub fn regexp_split_to_table(
    s: &str,
    p: &str,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<Vec<String>> {
    regexp_split(s, p, flags, ctype, "regexp_split_to_table()")
}

/// `regexp_split_to_array(s, p [, flags])`: the array's elements.
pub fn regexp_split_to_array(
    s: &str,
    p: &str,
    flags: Option<&str>,
    ctype: Ctype,
) -> Result<Vec<String>> {
    regexp_split(s, p, flags, ctype, "regexp_split_to_array()")
}

/// `regexp_substr(s, p [, start [, n [, flags [, subexpr]]]])`: the `n`th match from character
/// `start` on, or its `subexpr`th capture's match; None where there is no such match or the
/// capture matched nothing.
#[allow(clippy::too_many_arguments)]
pub fn regexp_substr(
    s: &str,
    p: &str,
    start: Option<i32>,
    n: Option<i32>,
    flags: Option<&str>,
    subexpr: Option<i32>,
    ctype: Ctype,
) -> Result<Option<String>> {
    let start = start_offset(start)?;
    let n = nth(n)?;
    let subexpr = subexpression(subexpr)?;
    let re_flags = every_match(flags, "regexp_substr()")?;

    let ctx = setup_regexp_matches(s, p, re_flags, start, ctype, subexpr > 0, false)?;

    if n > ctx.nmatches || subexpr > ctx.npatterns {
        return Ok(None);
    }
    let mut pos = (n - 1) * ctx.npatterns;
    if subexpr > 0 {
        pos += subexpr - 1;
    }
    pos *= 2;
    Ok(ctx.text(ctx.match_locs[pos], ctx.match_locs[pos + 1]))
}

#[cfg(test)]
mod tests {
    //! What PostgreSQL 19 answers, for what needs no compiled pattern: SIMILAR TO's translation,
    //! the flags, and the arguments refused before a pattern is compiled.

    use super::*;
    use crate::regex::guts::REG_EXPANDED;

    const C: Ctype = Ctype::C;

    fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
        r.unwrap_err().0
    }

    #[test]
    fn similar_to_escape_translates_as_postgresql_does() {
        let cases: &[(&str, Option<&str>, &str)] = &[
            ("abc", None, "^(?:abc)$"),
            ("a%b_c", None, "^(?:a.*b.c)$"),
            ("a(b|c)*", None, "^(?:a(?:b|c)*)$"),
            (r"a.b^c$d\e", None, r"^(?:a\.b\^c\$d\e)$"),
            (r#"%\"x\"%"#, None, "^(?:.*){1,1}?(x){1,1}(?:.*)$"),
            (r"a#%b\c", Some("#"), r"^(?:a\%b\\c)$"),
            (r"a\b%", Some(""), r"^(?:a\\b.*)$"),
            ("a€%b", Some("€"), r"^(?:a\%b)$"),
            ("é€é", Some("€"), r"^(?:é\é)$"),
            ("[%_]x", None, "^(?:[%_]x)$"),
            ("[]a]", None, "^(?:[]a])$"),
            ("[^]a]%", None, "^(?:[^]a].*)$"),
            (r"[\d]", None, r"^(?:[\d])$"),
            (r"[a\]", Some("#"), r"^(?:[a\\])$"),
            ("[[:alpha:]]%", None, "^(?:[[:alpha:]].*)$"),
            (r#"[\"]"#, None, r#"^(?:[\"])$"#),
            (r#"a\"b"#, None, "^(?:a){1,1}?(b)$"),
            ("é_ü%", Some(""), "^(?:é.ü.*)$"),
            (r"\é\%", None, r"^(?:\é\%)$"),
            ("", None, "^(?:)$"),
            (r"a\", None, "^(?:a)$"),
            ("[^^]", None, "^(?:[^^])$"),
            ("[a[b]c]%", None, "^(?:[a[b]c].*)$"),
        ];
        for &(pat, esc, want) in cases {
            assert_eq!(similar_to_escape(pat, esc).unwrap(), want, "{pat} {esc:?}");
        }
    }

    #[test]
    fn similar_escape_is_not_strict_and_similar_to_escape_refuses_as_postgresql_does() {
        assert_eq!(similar_escape(None, Some("x")).unwrap(), None);
        assert_eq!(
            similar_escape(Some("a%"), None).unwrap().as_deref(),
            Some("^(?:a.*)$")
        );
        assert_eq!(
            similar_escape(Some("a#%"), Some("#")).unwrap().as_deref(),
            Some(r"^(?:a\%)$")
        );
        assert_eq!(
            err(similar_to_escape(r#"\"a\"b\"c\""#, None)),
            "SQL regular expression may not contain more than two escape-double-quote separators"
        );
        assert_eq!(
            err(similar_to_escape("a", Some("ab"))),
            "invalid escape string"
        );
        assert_eq!(
            err(similar_to_escape("a", Some("é€"))),
            "invalid escape string"
        );
        assert_eq!(
            err(similar_escape(Some("a"), Some("xy"))),
            "invalid escape string"
        );
    }

    #[test]
    fn the_flags_set_the_compile_flags_as_postgresql_does() {
        let f = |o: Option<&str>| parse_re_flags(o).unwrap();
        assert_eq!(
            f(None),
            ReFlags {
                cflags: REG_ADVANCED,
                glob: false
            }
        );
        assert_eq!(
            f(Some("gi")),
            ReFlags {
                cflags: REG_ADVANCED | REG_ICASE,
                glob: true
            }
        );
        assert_eq!(f(Some("ic")).cflags, REG_ADVANCED);
        // e clears the advanced flag's bits, the extended one's among them.
        assert_eq!(f(Some("e")).cflags, 0);
        assert_eq!(f(Some("b")).cflags, 0);
        assert_eq!(f(Some("q")).cflags, REG_QUOTE);
        assert_eq!(f(Some("n")).cflags, REG_ADVANCED | REG_NEWLINE);
        assert_eq!(f(Some("m")).cflags, REG_ADVANCED | REG_NEWLINE);
        assert_eq!(f(Some("ms")).cflags, REG_ADVANCED);
        assert_eq!(f(Some("p")).cflags, REG_ADVANCED | REG_NLSTOP);
        assert_eq!(f(Some("w")).cflags, REG_ADVANCED | REG_NLANCH);
        assert_eq!(f(Some("np")).cflags, REG_ADVANCED | REG_NLSTOP);
        assert_eq!(f(Some("x")).cflags, REG_ADVANCED | REG_EXPANDED);
        assert_eq!(f(Some("xt")).cflags, REG_ADVANCED);
        assert_eq!(f(Some("")).cflags, REG_ADVANCED);
        assert_eq!(
            err(parse_re_flags(Some("k"))),
            "invalid regular expression option: \"k\""
        );
        assert_eq!(
            err(parse_re_flags(Some("gé"))),
            "invalid regular expression option: \"é\""
        );
    }

    #[test]
    fn arguments_out_of_range_are_refused_before_the_pattern_is_compiled() {
        // The pattern does not compile: each refusal comes first, as in PostgreSQL.
        let bad = "(";
        let cases: Vec<(String, &str)> = vec![
            (
                err(textregexreplace_extended("abc", bad, "X", 0, None, None, C)),
                "invalid value for parameter \"start\": 0",
            ),
            (
                err(textregexreplace_extended(
                    "abc",
                    bad,
                    "X",
                    1,
                    Some(-1),
                    None,
                    C,
                )),
                "invalid value for parameter \"n\": -1",
            ),
            (
                err(textregexreplace("abc", bad, "X", "1", C)),
                "invalid regular expression option: \"1\"",
            ),
            (
                err(textregexreplace("abc", bad, "X", "z", C)),
                "invalid regular expression option: \"z\"",
            ),
            (
                err(textregexreplace("abc", bad, "X", "éz", C)),
                "invalid regular expression option: \"é\"",
            ),
            (
                err(regexp_count("abc", bad, Some(0), None, C)),
                "invalid value for parameter \"start\": 0",
            ),
            (
                err(regexp_count("abc", bad, Some(1), Some("g"), C)),
                "regexp_count() does not support the \"global\" option",
            ),
            (
                err(regexp_count("abc", bad, Some(1), Some("k"), C)),
                "invalid regular expression option: \"k\"",
            ),
            (
                err(regexp_instr(
                    "abc",
                    bad,
                    Some(0),
                    Some(0),
                    None,
                    None,
                    None,
                    C,
                )),
                "invalid value for parameter \"start\": 0",
            ),
            (
                err(regexp_instr(
                    "abc",
                    bad,
                    Some(1),
                    Some(0),
                    None,
                    None,
                    None,
                    C,
                )),
                "invalid value for parameter \"n\": 0",
            ),
            (
                err(regexp_instr(
                    "abc",
                    bad,
                    Some(1),
                    Some(1),
                    Some(2),
                    None,
                    None,
                    C,
                )),
                "invalid value for parameter \"endoption\": 2",
            ),
            (
                err(regexp_instr(
                    "abc",
                    bad,
                    Some(1),
                    Some(1),
                    Some(0),
                    Some(""),
                    Some(-1),
                    C,
                )),
                "invalid value for parameter \"subexpr\": -1",
            ),
            (
                err(regexp_instr(
                    "abc",
                    bad,
                    Some(1),
                    Some(1),
                    Some(0),
                    Some("g"),
                    None,
                    C,
                )),
                "regexp_instr() does not support the \"global\" option",
            ),
            (
                err(regexp_like("abc", bad, Some("g"), C)),
                "regexp_like() does not support the \"global\" option",
            ),
            (
                err(regexp_match("abc", bad, Some("g"), C)),
                "regexp_match() does not support the \"global\" option",
            ),
            (
                err(regexp_matches("abc", bad, Some("z"), C)),
                "invalid regular expression option: \"z\"",
            ),
            (
                err(regexp_split_to_array("abc", bad, Some("g"), C)),
                "regexp_split_to_array() does not support the \"global\" option",
            ),
            (
                err(regexp_split_to_table("abc", bad, Some("g"), C)),
                "regexp_split_to_table() does not support the \"global\" option",
            ),
            (
                err(regexp_substr("abc", bad, Some(0), None, None, None, C)),
                "invalid value for parameter \"start\": 0",
            ),
            (
                err(regexp_substr("abc", bad, Some(1), Some(0), None, None, C)),
                "invalid value for parameter \"n\": 0",
            ),
            (
                err(regexp_substr(
                    "abc",
                    bad,
                    Some(1),
                    Some(1),
                    Some(""),
                    Some(-1),
                    C,
                )),
                "invalid value for parameter \"subexpr\": -1",
            ),
            (
                err(regexp_substr(
                    "abc",
                    bad,
                    Some(1),
                    Some(1),
                    Some("g"),
                    None,
                    C,
                )),
                "regexp_substr() does not support the \"global\" option",
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_replacement_s_backslashes_are_classified_as_postgresql_classifies_them() {
        assert_eq!(check_replace_text_has_escape(b"abc"), 0);
        assert_eq!(check_replace_text_has_escape(br"abc\"), 0);
        assert_eq!(check_replace_text_has_escape(br"a\&c"), 1);
        assert_eq!(check_replace_text_has_escape(br"a\\1"), 1);
        assert_eq!(check_replace_text_has_escape(br"a\x\9"), 2);
    }
}

#[cfg(test)]
mod with_the_compiler {
    //! What PostgreSQL 19 answers for each function over a compiled pattern. These wait for the
    //! compiler.

    use super::*;

    const C: Ctype = Ctype::C;
    const DB: Ctype = Ctype::Libc { default: true };

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    fn err<T: std::fmt::Debug>(r: Result<T>) -> String {
        r.unwrap_err().0
    }

    #[test]
    fn the_match_operators() {
        assert!(textregexeq("abc", "b", false, C).unwrap());
        assert!(!textregexeq("abc", "^b", false, C).unwrap());
        assert!(textregexeq("ABC", "b", true, C).unwrap());
        assert!(!textregexeq("ABC", "b", false, C).unwrap());
        assert!(!textregexeq("ÉCOLE", "école", true, C).unwrap());
        if DB.available() {
            assert!(textregexeq("ÉCOLE", "école", true, DB).unwrap());
        }
    }

    #[test]
    fn substring_by_a_pattern() {
        assert_eq!(textregexsubstr("foobar", "o.b", C).unwrap(), s("oob"));
        assert_eq!(textregexsubstr("foobar", "o(.)b", C).unwrap(), s("o"));
        assert_eq!(textregexsubstr("foo", "foo(bar)?", C).unwrap(), None);
        assert_eq!(textregexsubstr("abc", "x", C).unwrap(), None);
        assert_eq!(textregexsubstr("añob", "ñ(.)", C).unwrap(), s("o"));
        // SIMILAR's substring.
        let p = similar_to_escape("%#\"o_b#\"%", Some("#")).unwrap();
        assert_eq!(textregexsubstr("foobar", &p, C).unwrap(), s("oob"));
        assert!(textregexeq("abc", &similar_to_escape("a%", None).unwrap(), false, C).unwrap());
        assert!(!textregexeq("abc", &similar_to_escape("b%", None).unwrap(), false, C).unwrap());
    }

    #[test]
    fn regexp_replace_s_forms() {
        let r3 = |a, b, c| textregexreplace_noopt(a, b, c, C).unwrap();
        let r4 = |a, b, c, f| textregexreplace(a, b, c, f, C).unwrap();
        let rx = |a, b, c, st, n, f| textregexreplace_extended(a, b, c, st, n, f, C).unwrap();
        assert_eq!(r3("foobarbaz", "b..", "X"), "fooXbaz");
        assert_eq!(r4("foobarbaz", "b..", "X", "g"), "fooXX");
        assert_eq!(r4("foobarbaz", "b(..)", r"X\1Y", "g"), "fooXarYXazY");
        assert_eq!(r3("abc", "b", r"[\&]"), "a[b]c");
        assert_eq!(r3("abc", "b", r"\\"), r"a\c");
        assert_eq!(r3("abc", "b", r"\x"), r"a\xc");
        assert_eq!(r3("abc", "b", r"x\"), r"ax\c");
        assert_eq!(r3("abc", "(b)", r"\2"), "ac");
        assert_eq!(r4("abc", "", "-", "g"), "-a-b-c-");
        assert_eq!(r4("abc", "x*", "-", "g"), "-a-b-c-");
        assert_eq!(r3("añob", "ñ(o)", r"<\1>"), "a<o>b");
        assert_eq!(r4("abc", "B", "x", "gi"), "axc");
        assert_eq!(rx("abcabc", "b", "X", 3, None, None), "abcaXc");
        assert_eq!(rx("abcabc", "b", "X", 1, Some(2), None), "abcaXc");
        assert_eq!(rx("abcabc", "b", "X", 1, Some(0), None), "aXcaXc");
        assert_eq!(rx("abcabc", "B", "X", 1, Some(0), Some("i")), "aXcaXc");
        assert_eq!(rx("abc", "b", "X", 10, None, None), "abc");
        assert_eq!(rx("abcabc", "b", "X", 2, None, None), "aXcabc");
        assert_eq!(rx("ababab", "b", "X", 1, Some(2), Some("g")), "abaXab");
        assert_eq!(rx("abcabc", "b", "X", 1, Some(3), None), "abcabc");
        assert_eq!(r4("aaa", "a*", "X", "g"), "XX");
        assert_eq!(r3("abc", "(a)(b)(c)", r"\3\2\1\0"), r"cba\0");
        assert_eq!(r3("xyz", "y", r"\&\&"), "xyyz");
    }

    #[test]
    fn regexp_count_instr_like_and_substr() {
        let cnt = |a, b, st, f| regexp_count(a, b, st, f, C).unwrap();
        assert_eq!(cnt("abcabc", "b", None, None), 2);
        assert_eq!(cnt("abcabc", "b", Some(3), None), 1);
        assert_eq!(cnt("abcabc", "B", Some(1), Some("i")), 2);
        assert_eq!(cnt("abc", "", None, None), 4);
        assert_eq!(cnt("abc", "x", Some(10), None), 0);
        assert_eq!(cnt("aaa", "a*", None, None), 2);

        let ins = |a, b, st, n, e, f, sub| regexp_instr(a, b, st, n, e, f, sub, C).unwrap();
        assert_eq!(ins("abcabc", "b", None, None, None, None, None), 2);
        assert_eq!(ins("abcabc", "b", Some(1), Some(2), None, None, None), 5);
        assert_eq!(ins("abcabc", "b", Some(1), Some(2), Some(1), None, None), 6);
        assert_eq!(
            ins(
                "abcabc",
                "(b)(c)",
                Some(1),
                Some(1),
                Some(0),
                Some(""),
                Some(2)
            ),
            3
        );
        assert_eq!(ins("abcabc", "b", Some(1), Some(3), None, None, None), 0);
        assert_eq!(
            ins("abc", "b", Some(1), Some(1), Some(0), Some(""), Some(1)),
            2
        );
        assert_eq!(
            ins(
                "abc",
                "(b)?x|c",
                Some(1),
                Some(1),
                Some(0),
                Some(""),
                Some(1)
            ),
            0
        );
        assert_eq!(
            ins(
                "abcabc",
                "(b)(c)",
                Some(1),
                Some(1),
                Some(0),
                Some(""),
                Some(3)
            ),
            0
        );
        assert_eq!(
            ins("abcabc", "B", Some(4), Some(1), Some(1), Some("i"), None),
            6
        );

        let like = |a, b, f| regexp_like(a, b, f, C).unwrap();
        let cases = [
            ("abc", "B", Some("i"), true),
            ("abc", "B", None, false),
            ("abc", ".", Some("q"), false),
            ("a.c", ".", Some("q"), true),
            ("abc", "a b c", Some("x"), true),
            ("a\nb", "a$", Some("n"), true),
            ("a\nb", "a$", None, false),
            ("a+", "a+", Some("b"), true),
            ("aa", "^a+$", Some("b"), false),
            ("A", "a", Some("ic"), false),
            ("a\nb", "a.b", Some("p"), false),
            ("a\nb", "a.b", Some("w"), true),
            ("a\nb", "a$", Some("w"), true),
            ("a\nb", "a$", Some("p"), false),
            ("a\nb", "a.b", Some("ms"), true),
            ("abc", "a b c", Some("xt"), false),
            ("aa", "a{2}", Some("e"), false),
        ];
        for (a, b, f, want) in cases {
            assert_eq!(like(a, b, f), want, "{a:?} {b:?} {f:?}");
        }

        let sub = |a, b, st, n, f, x| regexp_substr(a, b, st, n, f, x, C).unwrap();
        assert_eq!(sub("abcabc", "b.", None, None, None, None), s("bc"));
        assert_eq!(sub("abcabd", "b.", Some(4), None, None, None), s("bd"));
        assert_eq!(sub("abcabd", "b.", Some(1), Some(2), None, None), s("bd"));
        assert_eq!(
            sub("abcabd", "(b)(.)", Some(1), Some(2), Some(""), Some(2)),
            s("d")
        );
        assert_eq!(sub("abc", "x", None, None, None, None), None);
        assert_eq!(sub("abc", "b", Some(1), Some(1), Some(""), Some(2)), None);
        assert_eq!(
            sub("abc", "(x)?b", Some(1), Some(1), Some(""), Some(1)),
            None
        );
        assert_eq!(sub("ABC", "b", Some(1), Some(1), Some("i"), None), s("B"));
        assert_eq!(sub("abc", "b", Some(1), Some(1), Some(""), Some(1)), s("b"));
    }

    #[test]
    fn regexp_match_and_matches() {
        let m = |a, b, f| regexp_match(a, b, f, C).unwrap();
        assert_eq!(
            m("foobarbequebaz", "bar.*que", None),
            Some(vec![s("barbeque")])
        );
        assert_eq!(
            m("foobarbequebaz", "(bar)(beque)", None),
            Some(vec![s("bar"), s("beque")])
        );
        assert_eq!(m("abc", "x", None), None);
        assert_eq!(m("abc", "(x)?b", None), Some(vec![None]));
        assert_eq!(m("abc", "B", Some("i")), Some(vec![s("b")]));
        assert_eq!(m("añob", "(ñ)(o)", None), Some(vec![s("ñ"), s("o")]));

        let ms = |a, b, f| regexp_matches(a, b, f, C).unwrap();
        assert_eq!(
            ms("foobarbequebazilbarfbonk", "(b[^b]+)(b[^b]+)", Some("g")),
            vec![vec![s("bar"), s("beque")], vec![s("bazil"), s("barf")]]
        );
        assert_eq!(ms("abc", "", Some("g")), vec![vec![s("")]; 4]);
        assert_eq!(ms("abcb", "b", None), vec![vec![s("b")]]);
        assert_eq!(
            ms("aaa", "a*", Some("g")),
            vec![vec![s("aaa")], vec![s("")]]
        );
        assert!(ms("abc", "x", Some("g")).is_empty());
        assert_eq!(
            ms("aXbxc", "(x)|(b)", Some("gi")),
            vec![vec![s("X"), None], vec![None, s("b")], vec![s("x"), None]]
        );
    }

    #[test]
    fn regexp_split() {
        let sp = |a, b, f| regexp_split_to_array(a, b, f, C).unwrap();
        assert_eq!(sp("hello world", r"\s+", None), ["hello", "world"]);
        assert_eq!(sp("abc", "", None), ["a", "b", "c"]);
        assert_eq!(sp("abc", "x*", None), ["a", "b", "c"]);
        assert_eq!(sp(" a ", " ", None), ["", "a", ""]);
        assert_eq!(sp("aXbxc", "x", Some("i")), ["a", "b", "c"]);
        assert_eq!(sp("", "x", None), [""]);
        assert_eq!(sp("abc", "abc", None), ["", ""]);
        assert_eq!(sp("añob", "", None), ["a", "ñ", "o", "b"]);
        assert_eq!(
            regexp_split_to_table("the quick  fox", r"\s+", None, C).unwrap(),
            ["the", "quick", "fox"]
        );
    }

    #[test]
    fn a_pattern_that_does_not_compile() {
        let cases: Vec<(String, &str)> = vec![
            (
                err(regexp_like("a", "(", None, C)),
                "invalid regular expression: parentheses () not balanced",
            ),
            (
                err(regexp_like("a", "[", None, C)),
                "invalid regular expression: brackets [] not balanced",
            ),
            (
                err(regexp_match("a", "a{3,2}", None, C)),
                "invalid regular expression: invalid repetition count(s)",
            ),
            (
                err(textregexeq("a", r"a\", false, C)),
                r"invalid regular expression: invalid escape \ sequence",
            ),
            (
                err(textregexeq("a", "*", false, C)),
                "invalid regular expression: quantifier operand invalid",
            ),
            (
                err(textregexeq("a", "a)", false, C)),
                "invalid regular expression: parentheses () not balanced",
            ),
            (
                err(textregexsubstr("a", "(", C)),
                "invalid regular expression: parentheses () not balanced",
            ),
            (
                err(textregexreplace_noopt("a", "(", "x", C)),
                "invalid regular expression: parentheses () not balanced",
            ),
            (
                err(regexp_split_to_array("a", "(", Some("x"), C)),
                "invalid regular expression: parentheses () not balanced",
            ),
        ];
        for (got, want) in cases {
            assert_eq!(got, want);
        }
    }

    #[test]
    fn a_compiled_pattern_is_kept_and_found_again() {
        for _ in 0..3 {
            assert!(textregexeq("abc", "b", false, C).unwrap());
        }
        // More patterns than are kept, then the first again.
        for i in 0..(MAX_CACHED_RES + 5) {
            let p = format!("a{{{i}}}");
            assert!(textregexeq(&"a".repeat(i), &p, false, C).unwrap());
        }
        assert!(textregexeq("abc", "b", false, C).unwrap());
        RE_ARRAY.with(|a| assert!(a.borrow().len() <= MAX_CACHED_RES));
    }
}
