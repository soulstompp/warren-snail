//! `ts_headline` as PostgreSQL 19 writes one: the text's tokens marked with the query's operands
//! they match (`hlparsetext`, ts_parse.c), the stretch of them the default parser's headline
//! chooses (`prsd_headline`, wparser_def.c), and the text written with its matches marked
//! (`generateHeadline`).

use super::config::{config_of, lexize, map, Config};
use super::{
    depth, execute, held_query, on_stack, parser, phrase_execute, phrase_output, Found, Item, Op,
    Ternary, BOTH, LEXEME_MAX, L_ONLY, POSITION_MAX, R_ONLY,
};
use crate::{Error, Result};

const SPACE: u8 = 12;
const TAG_T: u8 = 13;
const PROTOCOL: u8 = 14;
const URL_T: u8 = 5;
const NUMHWORD: u8 = 15;
const ASCIIHWORD: u8 = 16;
const HWORD: u8 = 17;
const SCIENTIFIC: u8 = 7;
const VERSIONNUMBER: u8 = 8;
const DECIMAL_T: u8 = 20;
const SIGNEDINT: u8 = 21;
const UNSIGNEDINT: u8 = 22;
const XMLENTITY: u8 = 23;

/// Tokens a headline passes over: a URL and the whole of a hyphenated word (HLIDSKIP).
fn skipped(t: u8) -> bool {
    matches!(t, URL_T | NUMHWORD | ASCIIHWORD | HWORD)
}

/// Tokens that are not words: NONWORDTOKEN.
fn nonword(t: u8) -> bool {
    t == SPACE || t == TAG_T || skipped(t)
}

/// Tokens a headline does not end at: NOENDTOKEN.
fn noend(t: u8) -> bool {
    nonword(t)
        || matches!(
            t,
            SCIENTIFIC
                | VERSIONNUMBER
                | DECIMAL_T
                | SIGNEDINT
                | UNSIGNEDINT
                | TAG_T
                | PROTOCOL
                | SPACE
                | XMLENTITY
        )
}

/// A token of the text as a headline holds it: `HeadlineWordEntry`.
#[derive(Clone, Default)]
struct HWord {
    kind: u8,
    word: String,
    pos: u16,
    item: Option<usize>,
    repeated: bool,
    selected: bool,
    within: bool,
    replace: bool,
    skip: bool,
}

/// The text's tokens, each a query's operand matches marked with it, a token matching more than
/// one repeated for each: `hlparsetext` and `hlfinditem` (ts_parse.c).
fn hlparsetext(c: Config, items: &[Item], text: &str) -> Result<Vec<HWord>> {
    let mut words: Vec<HWord> = Vec::new();
    let mut vectorpos: u32 = 0;
    for (tokid, token) in parser::parse(text) {
        if token.len() > LEXEME_MAX {
            continue;
        }
        let lexemes = match map(c, tokid) {
            Some(d) => lexize(d, &token)?,
            None => None,
        };
        let kind = tokid as u8;
        words.push(HWord {
            kind,
            word: token,
            ..HWord::default()
        });
        let Some(lexemes) = lexemes else {
            continue;
        };
        vectorpos += 1;
        for lexeme in lexemes {
            let last = words.len() - 1;
            words[last].pos = vectorpos.min(POSITION_MAX as u32) as u16;
            for (k, item) in items.iter().enumerate() {
                let Item::Val { word, prefix, .. } = item else {
                    continue;
                };
                let hit = if *prefix {
                    lexeme.as_bytes().starts_with(word.as_bytes())
                } else {
                    lexeme == *word
                };
                if !hit {
                    continue;
                }
                if words[last].item.is_some() {
                    let mut copy = words[last].clone();
                    copy.item = Some(k);
                    copy.repeated = true;
                    words.push(copy);
                } else {
                    words[last].item = Some(k);
                }
            }
        }
    }
    Ok(words)
}

/// Whether a run of tokens holds a token marked with operand `k`, its positions put in `data`
/// where asked for: `checkcondition_HL`.
fn check_hl(words: &[HWord], k: usize, data: Option<&mut Found>) -> Ternary {
    let mut data = data;
    for w in words {
        if w.item != Some(k) {
            continue;
        }
        match data.as_deref_mut() {
            None => return Ternary::Yes,
            Some(d) => {
                if d.pos.last().is_none_or(|&l| l < w.pos) {
                    d.pos.push(w.pos);
                }
            }
        }
    }
    match data {
        Some(d) if !d.pos.is_empty() => Ternary::Yes,
        _ => Ternary::No,
    }
}

/// Whether a run of tokens matches the query: `TS_execute` with `checkcondition_HL`.
fn hl_matches(items: &[Item], words: &[HWord]) -> bool {
    let cond = |k: usize, data: Option<&mut Found>| check_hl(words, k, data);
    execute(items, 0, &cond) != Ternary::No
}

/// The places each operand or phrase the query ANDs together is found at: `TS_execute_locations`
/// (tsvector_op.c); none where the query does not match.
fn locations(items: &[Item], i: usize, words: &[HWord]) -> Option<Vec<Found>> {
    let cond = |k: usize, data: Option<&mut Found>| check_hl(words, k, data);
    match &items[i] {
        Item::Val { .. } => {
            let mut d = Found::default();
            (cond(i, Some(&mut d)) == Ternary::Yes).then(|| vec![d])
        }
        Item::Opr { op, left, .. } => match op {
            Op::Not => match locations(items, i + 1, words) {
                None => Some(Vec::new()),
                Some(_) => None,
            },
            Op::And => {
                let mut l = locations(items, i + left, words)?;
                let r = locations(items, i + 1, words)?;
                l.extend(r);
                Some(l)
            }
            Op::Or => {
                let l = locations(items, i + left, words);
                let r = locations(items, i + 1, words);
                if l.is_none() && r.is_none() {
                    return None;
                }
                let (l, r) = (l.unwrap_or_default(), r.unwrap_or_default());
                if l.is_empty() {
                    return Some(r);
                }
                if r.is_empty() {
                    return Some(l);
                }
                let mut out = Vec::new();
                for ld in &l {
                    for rd in &r {
                        let mut d = Found::default();
                        phrase_output(Some(&mut d), ld, rd, BOTH | L_ONLY | R_ONLY, 0, 0);
                        d.width = ld.width.max(rd.width);
                        out.push(d);
                    }
                }
                Some(out)
            }
            Op::Phrase => {
                let mut d = Found::default();
                if phrase_execute(items, i, &cond, Some(&mut d)) == Ternary::Yes {
                    Some(if d.negate { Vec::new() } else { vec![d] })
                } else {
                    None
                }
            }
        },
    }
}

/// The next stretch of tokens from lexeme position `nextpos` holding every location, shortest
/// and earliest, that matches the query: `hlCover`. Its first and last tokens' indexes.
fn hl_cover(
    words: &[HWord],
    items: &[Item],
    locs: &[Found],
    nextpos: &mut i32,
) -> Option<(i32, i32)> {
    let mut pos = *nextpos;
    loop {
        let mut pose = -1;
        for pd in locs {
            let first = pd.pos.iter().map(|&p| p as i32).find(|&e| e >= pos)?;
            if first > pose {
                pose = first;
            }
        }
        if pose < 0 {
            return None;
        }
        let mut posb = i32::MAX - 1;
        for pd in locs {
            let last = pd
                .pos
                .iter()
                .rev()
                .map(|&p| p as i32 - pd.width)
                .find(|&s| s <= pose)
                .unwrap_or(-1);
            if last < posb {
                posb = last;
            }
        }
        posb = posb.max(pos);
        if posb <= pose {
            let (mut idxb, mut idxe) = (-1i32, -1i32);
            for (i, w) in words.iter().enumerate() {
                if w.item.is_none() {
                    continue;
                }
                if idxb < 0 && w.pos as i32 >= posb {
                    idxb = i as i32;
                }
                if w.pos as i32 <= pose {
                    idxe = i as i32;
                } else {
                    break;
                }
            }
            if idxb >= 0 && idxe >= idxb && hl_matches(items, &words[idxb as usize..=idxe as usize])
            {
                *nextpos = posb + 1;
                return Some((idxb, idxe));
            }
        }
        pos = posb + 1;
    }
}

/// The tokens from `start` to `end` marked as the headline's, each match selected, tags replaced
/// by a space and skipped tokens passed over unless all is highlighted: `mark_fragment`.
fn mark_fragment(words: &mut [HWord], all: bool, start: i32, end: i32) {
    for i in start.max(0)..=end {
        let w = &mut words[i as usize];
        if w.item.is_some() {
            w.selected = true;
        }
        if !all {
            if w.kind == TAG_T {
                w.replace = true;
            } else if skipped(w.kind) {
                w.skip = true;
            }
        } else if skipped(w.kind) {
            w.skip = true;
        }
        w.within = !w.repeated;
    }
}

/// The headline's choices: `prsd_headline`'s options.
struct Choice {
    all: bool,
    shortword: i32,
    min_words: i32,
    max_words: i32,
}

impl Choice {
    fn interesting(words: &[HWord], j: i32) -> bool {
        let w = &words[j as usize];
        w.item.is_some() && !w.repeated
    }

    fn bad_end(&self, words: &[HWord], j: i32) -> bool {
        let w = &words[j as usize];
        (noend(w.kind) || w.word.len() as i32 <= self.shortword) && !Self::interesting(words, j)
    }
}

fn is_word(words: &[HWord], i: i32) -> bool {
    !nonword(words[i as usize].kind)
}

/// The headline as one stretch of words around the best cover: `mark_hl_words`.
fn mark_hl_words(words: &mut [HWord], items: &[Item], locs: &[Found], ch: &Choice) {
    let n = words.len() as i32;
    let (mut bestb, mut beste) = (-1i32, -1i32);
    let mut bestlen = -1;
    let mut bestcover = false;
    if !ch.all {
        let mut nextpos = 0;
        while let Some((p, q)) = hl_cover(words, items, locs, &mut nextpos) {
            let w: &[HWord] = words;
            let (mut curlen, mut poslen) = (0, 0);
            let (mut posb, mut pose) = (p, p);
            let mut i = p;
            while i <= q && curlen < ch.max_words {
                if is_word(w, i) {
                    curlen += 1;
                }
                if Choice::interesting(w, i) {
                    poslen += 1;
                }
                pose = i;
                i += 1;
            }
            if curlen < ch.max_words {
                i -= 1;
                while i < n && curlen < ch.max_words {
                    if i > q {
                        if is_word(w, i) {
                            curlen += 1;
                        }
                        if Choice::interesting(w, i) {
                            poslen += 1;
                        }
                    }
                    pose = i;
                    if ch.bad_end(w, i) {
                        i += 1;
                        continue;
                    }
                    if curlen >= ch.min_words {
                        break;
                    }
                    i += 1;
                }
                if curlen < ch.min_words {
                    i = p - 1;
                    while i >= 0 {
                        if is_word(w, i) {
                            curlen += 1;
                        }
                        if Choice::interesting(w, i) {
                            poslen += 1;
                        }
                        if curlen >= ch.max_words {
                            break;
                        }
                        if ch.bad_end(w, i) {
                            i -= 1;
                            continue;
                        }
                        if curlen >= ch.min_words {
                            break;
                        }
                        i -= 1;
                    }
                    posb = if i >= 0 { i } else { 0 };
                }
            } else {
                if i > q {
                    i = q;
                }
                while curlen > ch.min_words {
                    if !ch.bad_end(w, i) {
                        break;
                    }
                    if is_word(w, i) {
                        curlen -= 1;
                    }
                    if Choice::interesting(w, i) {
                        poslen -= 1;
                    }
                    pose = i - 1;
                    i -= 1;
                }
            }
            let poscover = posb <= p && pose >= q;
            let better = (poscover && !bestcover)
                || (poscover == bestcover && poslen > bestlen)
                || (poscover == bestcover
                    && poslen == bestlen
                    && !ch.bad_end(w, pose)
                    && ch.bad_end(w, beste));
            if better {
                bestb = posb;
                beste = pose;
                bestlen = poslen;
                bestcover = poscover;
            }
        }
        if bestlen < 0 {
            let (mut curlen, mut pose) = (0, -1);
            let mut i = 0;
            while i < n && curlen < ch.min_words {
                if is_word(words, i) {
                    curlen += 1;
                }
                pose = i;
                i += 1;
            }
            bestb = 0;
            beste = pose;
        }
    } else {
        bestb = 0;
        beste = n - 1;
    }
    mark_fragment(words, ch.all, bestb, beste);
}

/// A fragment of a cover: `CoverPos`.
#[derive(Clone, Copy)]
struct CoverPos {
    startpos: i32,
    endpos: i32,
    poslen: i32,
    curlen: i32,
    chosen: bool,
    excluded: bool,
}

/// The next fragment of a cover, at most `max_words` words, from a match to a match:
/// `get_next_fragment`. Its words and its matches.
fn next_fragment(
    words: &[HWord],
    startpos: &mut i32,
    endpos: &mut i32,
    max_words: i32,
) -> (i32, i32) {
    let mut i = *startpos;
    while i <= *endpos {
        *startpos = i;
        if Choice::interesting(words, i) {
            break;
        }
        i += 1;
    }
    let (mut curlen, mut poslen) = (0, 0);
    i = *startpos;
    while i <= *endpos && curlen < max_words {
        if is_word(words, i) {
            curlen += 1;
        }
        if Choice::interesting(words, i) {
            poslen += 1;
        }
        i += 1;
    }
    if *endpos > i {
        *endpos = i;
        while i >= *startpos {
            *endpos = i;
            if Choice::interesting(words, i) {
                break;
            }
            if is_word(words, i) {
                curlen -= 1;
            }
            i -= 1;
        }
    }
    (curlen, poslen)
}

/// The headline as up to `max_fragments` fragments of covers, those with the most matches first:
/// `mark_hl_fragments`.
fn mark_hl_fragments(
    words: &mut [HWord],
    items: &[Item],
    locs: &[Found],
    ch: &Choice,
    max_fragments: i32,
) {
    let n = words.len() as i32;
    let mut covers: Vec<CoverPos> = Vec::new();
    let mut nextpos = 0;
    while let Some((p, q)) = hl_cover(words, items, locs, &mut nextpos) {
        let mut startpos = p;
        let mut endpos = q;
        while startpos <= endpos {
            let (curlen, poslen) = next_fragment(words, &mut startpos, &mut endpos, ch.max_words);
            covers.push(CoverPos {
                startpos,
                endpos,
                poslen,
                curlen,
                chosen: false,
                excluded: false,
            });
            startpos = endpos + 1;
            endpos = q;
        }
    }
    let mut num_f = 0;
    for _ in 0..max_fragments {
        let mut maxitems = 0;
        let mut minwords = i32::MAX;
        let mut min_i: i32 = -1;
        for (i, c) in covers.iter().enumerate() {
            if !c.chosen
                && !c.excluded
                && (maxitems < c.poslen || (maxitems == c.poslen && minwords > c.curlen))
            {
                maxitems = c.poslen;
                minwords = c.curlen;
                min_i = i as i32;
            }
        }
        if min_i < 0 {
            break;
        }
        let m = min_i as usize;
        covers[m].chosen = true;
        let mut startpos = covers[m].startpos;
        let mut endpos = covers[m].endpos;
        let mut curlen = covers[m].curlen;
        if curlen < ch.max_words {
            let maxstretch = (ch.max_words - curlen) / 2;
            let mut stretch = 0;
            let mut posmarker = startpos;
            let mut i = startpos - 1;
            while i >= 0 && stretch < maxstretch && !words[i as usize].within {
                if is_word(words, i) {
                    curlen += 1;
                    stretch += 1;
                }
                posmarker = i;
                i -= 1;
            }
            i = posmarker;
            while i < startpos && ch.bad_end(words, i) {
                if is_word(words, i) {
                    curlen -= 1;
                }
                i += 1;
            }
            startpos = i;
            posmarker = endpos;
            i = endpos + 1;
            while i < n && curlen < ch.max_words && !words[i as usize].within {
                if is_word(words, i) {
                    curlen += 1;
                }
                posmarker = i;
                i += 1;
            }
            i = posmarker;
            while i > endpos && ch.bad_end(words, i) {
                if is_word(words, i) {
                    curlen -= 1;
                }
                i -= 1;
            }
            endpos = i;
        }
        covers[m].startpos = startpos;
        covers[m].endpos = endpos;
        covers[m].curlen = curlen;
        mark_fragment(words, ch.all, startpos, endpos);
        num_f += 1;
        for (i, c) in covers.iter_mut().enumerate() {
            if i != m
                && ((c.startpos >= startpos && c.startpos <= endpos)
                    || (c.endpos >= startpos && c.endpos <= endpos)
                    || (c.startpos < startpos && c.endpos > endpos))
            {
                c.excluded = true;
            }
        }
    }
    if num_f <= 0 {
        let (mut curlen, mut endpos) = (0, -1);
        let mut i = 0;
        while i < n && curlen < ch.min_words {
            if is_word(words, i) {
                curlen += 1;
            }
            endpos = i;
            i += 1;
        }
        mark_fragment(words, ch.all, 0, endpos);
    }
}

/// A headline option's text read as an integer: `pg_strtoint32`.
fn int4(s: &str) -> Result<i32> {
    let bad = || Error::new(format!("invalid input syntax for type integer: \"{s}\""));
    let b = s.as_bytes();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c');
    let mut i = 0;
    while i < b.len() && space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'-' || b[i] == b'+') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut base = 10u32;
    if i + 1 < b.len() && b[i] == b'0' {
        match b[i + 1] {
            b'x' | b'X' => base = 16,
            b'o' | b'O' => base = 8,
            b'b' | b'B' => base = 2,
            _ => {}
        }
        if base != 10 {
            i += 2;
        }
    }
    let mut v: i64 = 0;
    let mut digits = 0;
    let mut over = false;
    while i < b.len() {
        let c = b[i];
        if c == b'_' {
            if digits == 0 || i + 1 >= b.len() || !(b[i + 1] as char).is_digit(base) {
                return Err(bad());
            }
            i += 1;
            continue;
        }
        let Some(d) = (c as char).to_digit(base) else {
            break;
        };
        v = v * base as i64 + d as i64;
        if v > 1 << 31 {
            over = true;
            v = 1 << 31;
        }
        digits += 1;
        i += 1;
    }
    if digits == 0 {
        return Err(bad());
    }
    while i < b.len() && space(b[i]) {
        i += 1;
    }
    if i < b.len() {
        return Err(bad());
    }
    let v = if neg { -v } else { v };
    if over || v > i32::MAX as i64 || v < i32::MIN as i64 {
        return Err(Error::new(format!(
            "value \"{s}\" is out of range for type integer"
        )));
    }
    Ok(v as i32)
}

/// A headline's options as `deserialize_deflist` (tsearchcmds.c) reads them: `name = value`
/// pairs, a value quoted with `'`, `E'` or `"`, or bare, a bare integer written back as
/// `defGetString` writes it.
fn options(text: &str) -> Result<Vec<(String, String)>> {
    #[derive(PartialEq)]
    enum S {
        WaitKey,
        InKey,
        InQKey,
        WaitEq,
        WaitValue,
        InSqValue,
        InDqValue,
        InWValue,
    }
    let wrong = || Error::new(format!("invalid parameter list format: \"{text}\""));
    let b = text.as_bytes();
    let space = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0b' | b'\x0c');
    let mut out = Vec::new();
    let mut key: Vec<u8> = Vec::new();
    let mut val: Vec<u8> = Vec::new();
    let mut state = S::WaitKey;
    let bare = |key: &[u8], val: &[u8]| -> (String, String) {
        let k = String::from_utf8_lossy(key).into_owned();
        let v = String::from_utf8_lossy(val).into_owned();
        let t = v.trim_start_matches(|c: char| c.is_ascii_whitespace());
        let digits = t.strip_prefix(['+', '-']).unwrap_or(t);
        if !v.is_empty() && !digits.is_empty() && digits.bytes().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = t.parse::<i64>() {
                if (i32::MIN as i64..=i32::MAX as i64).contains(&n) {
                    return (k, n.to_string());
                }
            }
        }
        (k, v)
    };
    let mut p = 0;
    while p < b.len() {
        let c = b[p];
        match state {
            S::WaitKey => {
                if !(space(c) || c == b',') {
                    key.clear();
                    if c == b'"' {
                        state = S::InQKey;
                    } else {
                        key.push(c);
                        state = S::InKey;
                    }
                }
            }
            S::InKey => {
                if space(c) {
                    state = S::WaitEq;
                } else if c == b'=' {
                    state = S::WaitValue;
                } else {
                    key.push(c);
                }
            }
            S::InQKey => {
                if c == b'"' {
                    if p + 1 < b.len() && b[p + 1] == b'"' {
                        key.push(c);
                        p += 1;
                    } else {
                        state = S::WaitEq;
                    }
                } else {
                    key.push(c);
                }
            }
            S::WaitEq => {
                if c == b'=' {
                    state = S::WaitValue;
                } else if !space(c) {
                    return Err(wrong());
                }
            }
            S::WaitValue => {
                val.clear();
                if c == b'\'' {
                    state = S::InSqValue;
                } else if c == b'E' && p + 1 < b.len() && b[p + 1] == b'\'' {
                    p += 1;
                    state = S::InSqValue;
                } else if c == b'"' {
                    state = S::InDqValue;
                } else if !space(c) {
                    val.push(c);
                    state = S::InWValue;
                }
            }
            S::InSqValue => {
                if c == b'\'' {
                    if p + 1 < b.len() && b[p + 1] == b'\'' {
                        val.push(c);
                        p += 1;
                    } else {
                        out.push((
                            String::from_utf8_lossy(&key).into_owned(),
                            String::from_utf8_lossy(&val).into_owned(),
                        ));
                        state = S::WaitKey;
                    }
                } else if c == b'\\' {
                    val.push(c);
                    if p + 1 < b.len() && b[p + 1] == b'\\' {
                        p += 1;
                    }
                } else {
                    val.push(c);
                }
            }
            S::InDqValue => {
                if c == b'"' {
                    if p + 1 < b.len() && b[p + 1] == b'"' {
                        val.push(c);
                        p += 1;
                    } else {
                        out.push((
                            String::from_utf8_lossy(&key).into_owned(),
                            String::from_utf8_lossy(&val).into_owned(),
                        ));
                        state = S::WaitKey;
                    }
                } else {
                    val.push(c);
                }
            }
            S::InWValue => {
                if c == b',' || space(c) {
                    out.push(bare(&key, &val));
                    state = S::WaitKey;
                } else {
                    val.push(c);
                }
            }
        }
        p += 1;
    }
    if state == S::InWValue {
        out.push(bare(&key, &val));
    } else if state != S::WaitKey {
        return Err(wrong());
    }
    Ok(out)
}

/// `ts_headline(regconfig, text, tsquery [, options])`: the text's best stretch around the
/// query's matches, each match between `StartSel` and `StopSel` (`ts_headline_byid_opt`,
/// wparser.c, with `prsd_headline`).
pub fn ts_headline(config: &str, text: &str, query: &str, opts: Option<&str>) -> Result<String> {
    let c = config_of(config)?;
    let items = held_query(query);
    let mut words = hlparsetext(c, &items, text)?;
    let mut ch = Choice {
        all: false,
        shortword: 3,
        min_words: 15,
        max_words: 35,
    };
    let mut max_fragments = 0;
    let (mut startsel, mut stopsel, mut fragdelim) = (None, None, None);
    for (name, val) in options(opts.unwrap_or(""))? {
        match name.to_ascii_lowercase().as_str() {
            "maxwords" => ch.max_words = int4(&val)?,
            "minwords" => ch.min_words = int4(&val)?,
            "shortword" => ch.shortword = int4(&val)?,
            "maxfragments" => max_fragments = int4(&val)?,
            "startsel" => startsel = Some(val),
            "stopsel" => stopsel = Some(val),
            "fragmentdelimiter" => fragdelim = Some(val),
            "highlightall" => {
                ch.all = ["1", "on", "true", "t", "y", "yes"]
                    .iter()
                    .any(|t| val.eq_ignore_ascii_case(t))
            }
            _ => {
                return Err(Error::new(format!(
                    "unrecognized headline parameter: \"{name}\""
                )))
            }
        }
    }
    if !ch.all {
        if ch.min_words >= ch.max_words {
            return Err(Error::new("MinWords must be less than MaxWords"));
        }
        if ch.min_words <= 0 {
            return Err(Error::new("MinWords must be positive"));
        }
        if ch.shortword < 0 {
            return Err(Error::new("ShortWord must be >= 0"));
        }
        if max_fragments < 0 {
            return Err(Error::new("MaxFragments must be >= 0"));
        }
    }
    on_stack(depth(&items), || {
        let locs = if items.is_empty() {
            Vec::new()
        } else {
            locations(&items, 0, &words).unwrap_or_default()
        };
        if max_fragments == 0 {
            mark_hl_words(&mut words, &items, &locs, &ch);
        } else {
            mark_hl_fragments(&mut words, &items, &locs, &ch, max_fragments);
        }
    });
    let startsel = startsel.unwrap_or_else(|| "<b>".into());
    let stopsel = stopsel.unwrap_or_else(|| "</b>".into());
    let fragdelim = fragdelim.unwrap_or_else(|| " ... ".into());
    for (s, name) in [
        (&startsel, "StartSel"),
        (&stopsel, "StopSel"),
        (&fragdelim, "FragmentDelimiter"),
    ] {
        if s.len() > i16::MAX as usize {
            return Err(Error::new(format!("value for \"{name}\" is too long")));
        }
    }
    let mut out = String::new();
    let mut infrag = false;
    let mut fragments = 0;
    for w in &words {
        if w.within && !w.repeated {
            if !infrag {
                infrag = true;
                fragments += 1;
                if fragments > 1 {
                    out.push_str(&fragdelim);
                }
            }
            if w.replace {
                out.push(' ');
            } else if !w.skip {
                if w.selected {
                    out.push_str(&startsel);
                }
                out.push_str(&w.word);
                if w.selected {
                    out.push_str(&stopsel);
                }
            }
        } else if !w.repeated {
            infrag = false;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::config::to_tsquery;
    use super::super::tsquery_in;
    use super::*;

    // Every expected value below is PostgreSQL 19's, read on its server.

    #[test]
    fn a_headline_is_chosen_and_marked_as_prsd_headline_does() {
        let en = |s: &str| to_tsquery("english", s).unwrap();
        let sea = "the sea and the ship ".repeat(60);
        assert_eq!(
            ts_headline(
                "english",
                &sea,
                &en("ship"),
                Some("MaxFragments=3, MaxWords=5, MinWords=2")
            )
            .unwrap(),
            "<b>ship</b> ... <b>ship</b> ... <b>ship</b>"
        );
        assert_eq!(
            ts_headline(
                "simple",
                "1 2 3 1 3",
                &tsquery_in("1 <-> 3").unwrap(),
                Some("MaxWords=2, MinWords=1")
            )
            .unwrap(),
            "<b>1</b> <b>3</b>"
        );
        assert_eq!(
            ts_headline(
                "english",
                "The <b>quick</b> http://x.org/a brown-fox fox",
                &en("fox | quick"),
                Some("MaxWords=4, MinWords=1")
            )
            .unwrap(),
            "<b>quick</b>"
        );
        assert_eq!(
            ts_headline("english", "fox fox fox fox", &en("fox & fox:*"), None).unwrap(),
            "<b>fox</b> <b>fox</b> <b>fox</b> <b>fox</b>"
        );
        assert_eq!(
            ts_headline(
                "english",
                "The quick brown fox",
                &en("fox"),
                Some("MaxWords=+010, MinWords=007")
            )
            .unwrap(),
            "The quick brown <b>fox</b>"
        );
        assert_eq!(
            ts_headline("english", "The <b>quick</b> brown fox", &en("fox"), None).unwrap(),
            "The  quick  brown <b>fox</b>"
        );
        assert_eq!(
            ts_headline("english", "The fox", &en("fox"), Some("StartSel=007")).unwrap(),
            "The 7fox</b>"
        );
        assert_eq!(
            ts_headline(
                "english",
                "alpha bravo charlie delta echo foxtrot fox golfs hotel india juliet kilos lima",
                &en("fox"),
                Some("MaxFragments=1, MaxWords=7, MinWords=1")
            )
            .unwrap(),
            "delta echo foxtrot <b>fox</b> golfs hotel india"
        );
        for (opts, msg) in [
            (
                "MinWords=5, MaxWords=5",
                "MinWords must be less than MaxWords",
            ),
            ("Bogus=1", "unrecognized headline parameter: \"Bogus\""),
            (
                "MaxWords = 5 MinWords",
                "invalid parameter list format: \"MaxWords = 5 MinWords\"",
            ),
            ("MaxWords=x", "invalid input syntax for type integer: \"x\""),
        ] {
            assert_eq!(
                ts_headline("english", "a fox", &en("fox"), Some(opts))
                    .unwrap_err()
                    .0,
                msg
            );
        }
    }
}
