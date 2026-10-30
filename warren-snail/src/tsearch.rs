//! Text search: the tsvector, a document's lexemes in order, each with the positions it was found
//! at and their weights, and the tsquery, a tree of lexemes under `!`, `&`, `|` and the phrase
//! operator `<N>`. Each is read and written as PostgreSQL's input and output functions read and
//! write it, keyed as its comparison orders it, and matched as `@@` matches.
//!
//! A tsvector is held as the text `tsvectorout` writes. A tsquery is held as the text `tsqueryout`
//! writes, but with parentheses where that text would read back as another tree or as none: around
//! the right operand of `&` when it is an `&` itself, and of `|` when it is an `|`, and around a
//! `!` that a run of `!` would push past the operators one level of parentheses holds.

use crate::places::varying_part;
use crate::{Error, Result};
use std::cmp::Ordering;

/// The most bytes a lexeme holds: MAXSTRLEN.
const LEXEME_MAX: usize = (1 << 11) - 1;

/// The most bytes a tsvector's lexemes and positions take, and the furthest into a tsquery's
/// operands one starts: MAXSTRPOS.
const STRING_MAX: usize = (1 << 20) - 1;

/// The most positions a lexeme keeps: MAXNUMPOS.
const POSITIONS_MAX: usize = 256;

/// The greatest position, where every greater one is clamped: MAXENTRYPOS less one.
const POSITION_MAX: u16 = (1 << 14) - 1;

/// The greatest distance of a phrase operator: MAXENTRYPOS.
const DISTANCE_MAX: i64 = 1 << 14;

/// The most operators one level of parentheses holds while a tsquery is read: STACKDEPTH.
const STACK_DEPTH: usize = 32;

/// The most bytes PostgreSQL allocates at once: MaxAllocSize.
const ALLOC_MAX: usize = 0x3fff_ffff;

/// The bytes before a tsvector's entries or a tsquery's items: the varlena header and the count.
const HEADER: usize = 8;

/// The bytes of a tsvector's entry (WordEntry) and of a tsquery's item (QueryItem).
const ENTRY: usize = 4;
const ITEM: usize = 12;

/// Whether C's `isspace` takes the byte.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r')
}

/// Whether a tsquery's lexeme ends at the byte: ISOPERATOR.
fn is_operator(c: u8) -> bool {
    matches!(c, b'!' | b'&' | b'|' | b'(' | b')' | b'<')
}

/// The bytes of the character that starts with byte `c`, as `pg_utf_mblen` reads them.
fn char_len(c: u8) -> usize {
    if c & 0x80 == 0 {
        1
    } else if c & 0xe0 == 0xc0 {
        2
    } else if c & 0xf0 == 0xe0 {
        3
    } else if c & 0xf8 == 0xf0 {
        4
    } else {
        1
    }
}

/// The byte at `i`, or 0 past the end, as a C string reads.
fn at(b: &[u8], i: usize) -> u8 {
    b.get(i).copied().unwrap_or(0)
}

/// Text as an input function receives it: up to its first zero byte.
fn cstring(s: &str) -> &str {
    s.find('\0').map_or(s, |i| &s[..i])
}

/// The position of a WordEntryPos: WEP_GETPOS.
fn pos_of(p: u16) -> u16 {
    p & 0x3fff
}

/// The weight of a WordEntryPos, 3 for A down to 0 for D: WEP_GETWEIGHT.
fn weight_of(p: u16) -> u16 {
    p >> 14
}

/// Sorts as `pg_qsort` and `qsort_arg` sort (src/include/lib/sort_template.h), so that elements
/// `cmp` finds equal end in the order PostgreSQL leaves them.
fn pg_qsort<T>(mut v: &mut [T], cmp: &dyn Fn(&T, &T) -> Ordering) {
    let med3 = |v: &[T], a: usize, b: usize, c: usize| -> usize {
        if cmp(&v[a], &v[b]).is_lt() {
            if cmp(&v[b], &v[c]).is_lt() {
                b
            } else if cmp(&v[a], &v[c]).is_lt() {
                c
            } else {
                a
            }
        } else if cmp(&v[b], &v[c]).is_gt() {
            b
        } else if cmp(&v[a], &v[c]).is_lt() {
            a
        } else {
            c
        }
    };
    loop {
        let n = v.len();
        if n < 7 {
            for pm in 1..n {
                let mut pl = pm;
                while pl > 0 && cmp(&v[pl - 1], &v[pl]).is_gt() {
                    v.swap(pl, pl - 1);
                    pl -= 1;
                }
            }
            return;
        }
        if (1..n).all(|pm| !cmp(&v[pm - 1], &v[pm]).is_gt()) {
            return;
        }
        let mut pm = n / 2;
        if n > 7 {
            let mut pl = 0;
            let mut pn = n - 1;
            if n > 40 {
                let d = n / 8;
                pl = med3(v, pl, pl + d, pl + 2 * d);
                pm = med3(v, pm - d, pm, pm + d);
                pn = med3(v, pn - 2 * d, pn - d, pn);
            }
            pm = med3(v, pl, pm, pn);
        }
        v.swap(0, pm);
        let (mut pa, mut pb) = (1, 1);
        let (mut pc, mut pd) = (n - 1, n - 1);
        loop {
            while pb <= pc {
                let r = cmp(&v[pb], &v[0]);
                if r.is_gt() {
                    break;
                }
                if r.is_eq() {
                    v.swap(pa, pb);
                    pa += 1;
                }
                pb += 1;
            }
            while pb <= pc {
                let r = cmp(&v[pc], &v[0]);
                if r.is_lt() {
                    break;
                }
                if r.is_eq() {
                    v.swap(pc, pd);
                    pd -= 1;
                }
                pc -= 1;
            }
            if pb > pc {
                break;
            }
            v.swap(pb, pc);
            pb += 1;
            pc -= 1;
        }
        let d1 = pa.min(pb - pa);
        for k in 0..d1 {
            v.swap(k, pb - d1 + k);
        }
        let d1 = (pd - pc).min(n - pd - 1);
        for k in 0..d1 {
            v.swap(pb + k, n - d1 + k);
        }
        let d1 = pb - pa;
        let d2 = pd - pc;
        let whole = v;
        if d1 <= d2 {
            pg_qsort(&mut whole[..d1], cmp);
            v = &mut whole[n - d2..];
        } else {
            pg_qsort(&mut whole[n - d2..], cmp);
            v = &mut whole[..d1];
        }
    }
}

/// A lexeme of a tsvector and its positions, each as a WordEntryPos holds it: the weight in the
/// top two bits, the position below; none where it has no positions.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    word: String,
    pos: Vec<u16>,
}

/// A lexeme as `gettoken_tsvector` reads one: its bytes, the positions a tsvector gives it, and
/// the byte after it.
struct Token {
    word: String,
    pos: Vec<u16>,
    end: usize,
}

/// A position as `atoi` reads the digits at the start of `b` (`strtol`'s long, cut to an int),
/// clamped as LIMITPOS clamps it, in the bits WEP_SETPOS keeps.
fn position(b: &[u8]) -> u16 {
    let mut v: i64 = 0;
    for &d in b.iter().take_while(|d| d.is_ascii_digit()) {
        match v
            .checked_mul(10)
            .and_then(|x| x.checked_add((d - b'0') as i64))
        {
            Some(x) => v = x,
            None => {
                v = i64::MAX;
                break;
            }
        }
    }
    let v = v as i32;
    let v = if v >= 1 << 14 { (1 << 14) - 1 } else { v };
    (v & 0x3fff) as u16
}

/// The next lexeme of `s` from byte `i`, as `gettoken_tsvector` (tsvector_parser.c) reads one for
/// a tsvector, or for a tsquery where `query`: none at the end of `s`.
fn token(s: &str, mut i: usize, query: bool) -> Result<Option<Token>> {
    #[derive(Clone, Copy)]
    enum State {
        Word,
        EndWord,
        NextChar,
        EndComplex,
        CharComplex,
        PosInfo,
        InPosInfo,
        PosDelim,
    }
    let b = s.as_bytes();
    let syntax = || {
        Error::new(format!(
            "syntax error in {}: \"{s}\"",
            if query { "tsquery" } else { "tsvector" }
        ))
    };
    let mut word: Vec<u8> = Vec::new();
    let mut pos: Vec<u16> = Vec::new();
    let mut state = State::Word;
    let mut back = State::EndWord;
    let done = |word: Vec<u8>, pos: Vec<u16>, end: usize| {
        let word = String::from_utf8(word).expect("a lexeme of whole characters");
        Ok(Some(Token { word, pos, end }))
    };
    loop {
        let c = at(b, i);
        let len = char_len(c);
        let copy = |word: &mut Vec<u8>| word.extend_from_slice(&b[i..i + len]);
        match state {
            State::Word => {
                if c == 0 {
                    return Ok(None);
                } else if c == b'\'' {
                    state = State::EndComplex;
                } else if c == b'\\' {
                    state = State::NextChar;
                    back = State::EndWord;
                } else if query && is_operator(c) {
                    return Err(syntax());
                } else if !is_space(c) {
                    copy(&mut word);
                    state = State::EndWord;
                }
            }
            State::NextChar => {
                if c == 0 {
                    return Err(Error::new(format!(
                        "there is no escaped character: \"{s}\""
                    )));
                }
                copy(&mut word);
                state = back;
            }
            State::EndWord => {
                if c == b'\\' {
                    state = State::NextChar;
                    back = State::EndWord;
                } else if is_space(c) || c == 0 || (query && is_operator(c)) {
                    if word.is_empty() {
                        return Err(syntax());
                    }
                    return done(word, pos, i);
                } else if c == b':' {
                    if word.is_empty() {
                        return Err(syntax());
                    }
                    if query {
                        return done(word, pos, i);
                    }
                    state = State::InPosInfo;
                } else {
                    copy(&mut word);
                }
            }
            State::EndComplex => {
                if c == b'\'' {
                    state = State::CharComplex;
                } else if c == b'\\' {
                    state = State::NextChar;
                    back = State::EndComplex;
                } else if c == 0 {
                    return Err(syntax());
                } else {
                    copy(&mut word);
                }
            }
            State::CharComplex => {
                if c == b'\'' {
                    word.push(b'\'');
                    state = State::EndComplex;
                } else {
                    if word.is_empty() {
                        return Err(syntax());
                    }
                    if query {
                        return done(word, pos, i);
                    }
                    state = State::PosInfo;
                    continue;
                }
            }
            State::PosInfo => {
                if c == b':' {
                    state = State::InPosInfo;
                } else {
                    return done(word, pos, i);
                }
            }
            State::InPosInfo => {
                if !c.is_ascii_digit() {
                    return Err(syntax());
                }
                let p = position(&b[i..]);
                if p == 0 {
                    return Err(Error::new(format!(
                        "wrong position info in tsvector: \"{s}\""
                    )));
                }
                pos.push(p);
                state = State::PosDelim;
            }
            State::PosDelim => {
                let weight = match c {
                    b'a' | b'A' | b'*' => Some(3),
                    b'b' | b'B' => Some(2),
                    b'c' | b'C' => Some(1),
                    b'd' | b'D' => Some(0),
                    _ => None,
                };
                if c == b',' {
                    state = State::InPosInfo;
                } else if let Some(w) = weight {
                    let last = pos.last_mut().expect("a position before its weight");
                    if weight_of(*last) != 0 {
                        return Err(syntax());
                    }
                    *last = w << 14 | pos_of(*last);
                } else if is_space(c) || c == 0 {
                    return done(word, pos, i);
                } else if !c.is_ascii_digit() {
                    return Err(syntax());
                }
            }
        }
        i += len;
    }
}

/// A lexeme's positions sorted by position, one kept of each, of the weight greatest among them,
/// and at most MAXNUMPOS of them, ending at the greatest position: `uniquePos` (tsvector.c).
fn unique_pos(a: &mut Vec<u16>) {
    if a.len() <= 1 {
        return;
    }
    pg_qsort(a, &|x, y| pos_of(*x).cmp(&pos_of(*y)));
    let mut res = 0;
    for ptr in 1..a.len() {
        if pos_of(a[ptr]) != pos_of(a[res]) {
            res += 1;
            a[res] = a[ptr];
            if res >= POSITIONS_MAX - 1 || pos_of(a[res]) == POSITION_MAX {
                break;
            }
        } else if weight_of(a[ptr]) > weight_of(a[res]) {
            a[res] = weight_of(a[ptr]) << 14 | pos_of(a[res]);
        }
    }
    a.truncate(res + 1);
}

/// Lexemes sorted by their bytes and made unique, the positions of each lexeme gathered and made
/// unique: `uniqueentry` (tsvector.c).
fn unique_entries(a: &mut Vec<Entry>) {
    if a.is_empty() {
        return;
    }
    pg_qsort(a, &|x, y| x.word.as_bytes().cmp(y.word.as_bytes()));
    let mut res = 0;
    for ptr in 1..a.len() {
        if a[ptr].word != a[res].word {
            unique_pos(&mut a[res].pos);
            res += 1;
            a.swap(res, ptr);
        } else if !a[ptr].pos.is_empty() {
            let moved = std::mem::take(&mut a[ptr].pos);
            a[res].pos.extend(moved);
        }
    }
    unique_pos(&mut a[res].pos);
    a.truncate(res + 1);
}

/// The bytes a tsvector's lexemes and positions take after its entries: each lexeme, then where it
/// has positions, a pad byte to an even offset, their count and the positions, two bytes each.
fn data_bytes(entries: &[Entry]) -> usize {
    entries.iter().fold(0, |n, e| {
        let n = n + e.word.len();
        if e.pos.is_empty() {
            n
        } else {
            n.next_multiple_of(2) + 2 + 2 * e.pos.len()
        }
    })
}

fn too_long(n: usize) -> Error {
    Error::new(format!(
        "string is too long for tsvector ({n} bytes, max {STRING_MAX} bytes)"
    ))
}

/// A tsvector read as `tsvectorin` reads one: its lexemes sorted and made unique.
fn parse_vector(s: &str) -> Result<Vec<Entry>> {
    let s = cstring(s);
    let mut entries = Vec::new();
    let mut total = 0;
    let mut i = 0;
    while let Some(t) = token(s, i, false)? {
        if t.word.len() > LEXEME_MAX {
            return Err(Error::new(format!(
                "word is too long ({} bytes, max {LEXEME_MAX} bytes)",
                t.word.len()
            )));
        }
        if total > STRING_MAX {
            return Err(too_long(total));
        }
        total += t.word.len();
        i = t.end;
        entries.push(Entry {
            word: t.word,
            pos: t.pos,
        });
    }
    unique_entries(&mut entries);
    let n = data_bytes(&entries);
    if n > STRING_MAX {
        return Err(too_long(n));
    }
    Ok(entries)
}

/// A tsvector written as `tsvectorout` writes one: each lexeme quoted, `'` and `\` doubled, then
/// its positions after `:`, each with its weight's letter but D's.
fn vector_text(entries: &[Entry]) -> String {
    let mut out = String::new();
    for (k, e) in entries.iter().enumerate() {
        if k > 0 {
            out.push(' ');
        }
        quoted(&mut out, &e.word);
        for (j, &p) in e.pos.iter().enumerate() {
            out.push(if j == 0 { ':' } else { ',' });
            out.push_str(&pos_of(p).to_string());
            match weight_of(p) {
                3 => out.push('A'),
                2 => out.push('B'),
                1 => out.push('C'),
                _ => {}
            }
        }
    }
    out
}

/// A lexeme quoted, `'` and `\` doubled, as `tsvectorout` and `tsqueryout` write one.
fn quoted(out: &mut String, word: &str) {
    out.push('\'');
    for ch in word.chars() {
        if ch == '\'' || ch == '\\' {
            out.push(ch);
        }
        out.push(ch);
    }
    out.push('\'');
}

/// The lexemes of a tsvector the snail holds.
fn held_vector(text: &str) -> Vec<Entry> {
    parse_vector(text).expect("a tsvector the snail holds")
}

/// A tsvector read as `tsvectorin` reads one (tsvector.c, tsvector_parser.c), as `tsvectorout`
/// writes it: its lexemes sorted by their bytes and made unique, each lexeme's positions sorted,
/// made unique at the greatest weight among them, clamped at 16383, and at most 256 of them.
pub fn tsvector_in(s: &str) -> Result<String> {
    Ok(vector_text(&parse_vector(s)?))
}

/// The bytes PostgreSQL lays a tsvector out in, its header among them: VARSIZE, as `tsvectorin`
/// lays one out (CALCDATASIZE).
pub fn tsvector_size(text: &str) -> usize {
    let entries = held_vector(text);
    HEADER + ENTRY * entries.len() + data_bytes(&entries)
}

/// The key of a tsvector, ordered as `tsvector_cmp` orders them (`silly_cmp_tsvector`,
/// tsvector_op.c), equal exactly where it finds them equal: the value's size and its count of
/// lexemes in four bytes each, big-endian; then for each lexeme a byte, 0 where it has positions,
/// 1 where not, its bytes as a part of varying length, and where it has positions, their count
/// taken from 0xFFFF, then each position, shifted up two bits past its weight, taken from 0xFFFF,
/// in two bytes each, big-endian.
pub fn tsvector_key(text: &str) -> Vec<u8> {
    let entries = held_vector(text);
    let size = HEADER + ENTRY * entries.len() + data_bytes(&entries);
    let mut out = Vec::with_capacity(size + 8);
    out.extend_from_slice(&(size as u32).to_be_bytes());
    out.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for e in &entries {
        out.push(if e.pos.is_empty() { 1 } else { 0 });
        varying_part(&mut out, e.word.as_bytes());
        if !e.pos.is_empty() {
            out.extend_from_slice(&(u16::MAX - e.pos.len() as u16).to_be_bytes());
            for &p in &e.pos {
                let at = pos_of(p) << 2 | weight_of(p);
                out.extend_from_slice(&(u16::MAX - at).to_be_bytes());
            }
        }
    }
    out
}

/// Two tsvectors joined as `||` joins them (`tsvector_concat`, tsvector_op.c): every lexeme of
/// either, the second's positions moved past the first's greatest, clamped at 16383, and a
/// lexeme in both keeping the first's positions, then the second's while there are fewer than
/// 256 and the last is short of 16383.
pub fn tsvector_concat(a: &str, b: &str) -> Result<String> {
    let a = held_vector(a);
    let b = held_vector(b);
    let maxpos = a
        .iter()
        .flat_map(|e| e.pos.iter().map(|&p| pos_of(p)))
        .max()
        .unwrap_or(0);
    let add = |dest: &mut Vec<u16>, src: &[u16]| {
        for &p in src {
            if dest.len() >= POSITIONS_MAX
                || dest.last().is_some_and(|&l| pos_of(l) == POSITION_MAX)
            {
                break;
            }
            let at = (pos_of(p) as u32 + maxpos as u32).min(POSITION_MAX as u32) as u16;
            dest.push(weight_of(p) << 14 | at);
        }
    };
    let mut out: Vec<Entry> = Vec::with_capacity(a.len() + b.len());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        let order = match (a.get(i), b.get(j)) {
            (Some(x), Some(y)) => x.word.as_bytes().cmp(y.word.as_bytes()),
            (Some(_), None) => Ordering::Less,
            _ => Ordering::Greater,
        };
        match order {
            Ordering::Less => {
                out.push(a[i].clone());
                i += 1;
            }
            Ordering::Greater => {
                let mut pos = Vec::new();
                add(&mut pos, &b[j].pos);
                out.push(Entry {
                    word: b[j].word.clone(),
                    pos,
                });
                j += 1;
            }
            Ordering::Equal => {
                let mut pos = a[i].pos.clone();
                add(&mut pos, &b[j].pos);
                out.push(Entry {
                    word: a[i].word.clone(),
                    pos,
                });
                i += 1;
                j += 1;
            }
        }
    }
    let n = data_bytes(&out);
    if n > STRING_MAX {
        return Err(too_long(n));
    }
    Ok(vector_text(&out))
}

/// A tsquery's operator, by its code in QueryOperator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Op {
    Not = 1,
    And = 2,
    Or = 3,
    Phrase = 4,
}

impl Op {
    /// How tightly the operator binds: `tsearch_op_priority`.
    fn priority(self) -> i32 {
        match self {
            Op::Not => 4,
            Op::And => 2,
            Op::Or => 1,
            Op::Phrase => 3,
        }
    }
}

/// An item of a tsquery, in the order PostgreSQL lays them out: an operator, then its right
/// operand's items, then its left operand's, `left` items after it; or an operand, with the
/// weights it asks for (A 8, B 4, C 2, D 1, none 0) and whether it is a prefix.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Val {
        word: String,
        weight: u8,
        prefix: bool,
    },
    Opr {
        op: Op,
        distance: u16,
        left: usize,
    },
}

/// A token of a tsquery as `gettoken_query_standard` (tsquery.c) gives one.
enum Tok {
    End,
    Val {
        word: String,
        weight: u8,
        prefix: bool,
    },
    Opr(Op, u16),
    Open,
    Close,
}

/// Where `gettoken_query_standard` is in a tsquery: before its first operand, before another, or
/// after one.
#[derive(Clone, Copy, PartialEq)]
enum Wait {
    First,
    Operand,
    Operator,
}

/// The reader of a tsquery's text.
struct Reader<'a> {
    s: &'a str,
    i: usize,
    wait: Wait,
    count: i64,
}

impl Reader<'_> {
    fn syntax(&self) -> Error {
        Error::new(format!("syntax error in tsquery: \"{}\"", self.s))
    }

    /// The weights and prefix flag after an operand, as `get_modifiers` reads them.
    fn modifiers(&mut self) -> (u8, bool) {
        let b = self.s.as_bytes();
        let (mut weight, mut prefix) = (0, false);
        if at(b, self.i) != b':' {
            return (weight, prefix);
        }
        self.i += 1;
        while at(b, self.i) != 0 && char_len(at(b, self.i)) == 1 {
            match at(b, self.i) {
                b'a' | b'A' => weight |= 1 << 3,
                b'b' | b'B' => weight |= 1 << 2,
                b'c' | b'C' => weight |= 1 << 1,
                b'd' | b'D' => weight |= 1,
                b'*' => prefix = true,
                _ => return (weight, prefix),
            }
            self.i += 1;
        }
        (weight, prefix)
    }

    /// A phrase operator, `<->` or `<N>`, as `parse_phrase_operator` reads one: its distance.
    fn phrase(&mut self) -> Result<Option<u16>> {
        let b = self.s.as_bytes();
        #[derive(PartialEq)]
        enum State {
            Open,
            Dist,
            Close,
            Finish,
        }
        let mut state = State::Open;
        let mut ptr = self.i;
        let mut l: i64 = 1;
        while at(b, ptr) != 0 {
            match state {
                State::Open => {
                    if at(b, ptr) != b'<' {
                        return Ok(None);
                    }
                    state = State::Dist;
                    ptr += 1;
                }
                State::Dist => {
                    if at(b, ptr) == b'-' {
                        state = State::Close;
                        ptr += 1;
                        continue;
                    }
                    if !at(b, ptr).is_ascii_digit() {
                        return Ok(None);
                    }
                    let digits = b[ptr..].iter().take_while(|d| d.is_ascii_digit()).count();
                    let v = self.s[ptr..ptr + digits].parse::<i64>().ok();
                    match v {
                        Some(v) if v <= DISTANCE_MAX => l = v,
                        _ => {
                            return Err(Error::new(format!(
                                "distance in phrase operator must be an integer value between zero and {DISTANCE_MAX} inclusive"
                            )))
                        }
                    }
                    state = State::Close;
                    ptr += digits;
                }
                State::Close => {
                    if at(b, ptr) != b'>' {
                        return Ok(None);
                    }
                    state = State::Finish;
                    ptr += 1;
                }
                State::Finish => {
                    self.i = ptr;
                    return Ok(Some(l as u16));
                }
            }
        }
        Ok(None)
    }

    /// The next token, as `gettoken_query_standard` reads it.
    fn next(&mut self) -> Result<Tok> {
        let b = self.s.as_bytes();
        loop {
            let c = at(b, self.i);
            match self.wait {
                Wait::First | Wait::Operand => {
                    if c == b'!' {
                        self.i += 1;
                        self.wait = Wait::Operand;
                        return Ok(Tok::Opr(Op::Not, 0));
                    } else if c == b'(' {
                        self.i += 1;
                        self.wait = Wait::Operand;
                        self.count += 1;
                        return Ok(Tok::Open);
                    } else if c == b':' {
                        return Err(self.syntax());
                    } else if !is_space(c) {
                        return match token(self.s, self.i, true)? {
                            Some(t) => {
                                self.i = t.end;
                                let (weight, prefix) = self.modifiers();
                                self.wait = Wait::Operator;
                                Ok(Tok::Val {
                                    word: t.word,
                                    weight,
                                    prefix,
                                })
                            }
                            None if self.wait == Wait::First => Ok(Tok::End),
                            None => {
                                Err(Error::new(format!("no operand in tsquery: \"{}\"", self.s)))
                            }
                        };
                    }
                }
                Wait::Operator => {
                    if c == b'&' {
                        self.i += 1;
                        self.wait = Wait::Operand;
                        return Ok(Tok::Opr(Op::And, 0));
                    } else if c == b'|' {
                        self.i += 1;
                        self.wait = Wait::Operand;
                        return Ok(Tok::Opr(Op::Or, 0));
                    } else if let Some(d) = self.phrase()? {
                        self.wait = Wait::Operand;
                        return Ok(Tok::Opr(Op::Phrase, d));
                    } else if c == b')' {
                        self.i += 1;
                        self.count -= 1;
                        return if self.count < 0 {
                            Err(self.syntax())
                        } else {
                            Ok(Tok::Close)
                        };
                    } else if c == 0 {
                        return if self.count != 0 {
                            Err(self.syntax())
                        } else {
                            Ok(Tok::End)
                        };
                    } else if !is_space(c) {
                        return Err(self.syntax());
                    }
                }
            }
            self.i += char_len(c);
        }
    }
}

/// The operators of one level of parentheses moved to the output, from the last, while they bind
/// at least as tightly as `op` (a `!` only more tightly than another `!`): `cleanOpStack`.
fn clean(stack: &mut Vec<(Op, u16)>, out: &mut Vec<Item>, op: Op) {
    while let Some(&(top, distance)) = stack.last() {
        if (op != Op::Not && op.priority() > top.priority())
            || (op == Op::Not && op.priority() >= top.priority())
        {
            break;
        }
        stack.pop();
        out.push(Item::Opr {
            op: top,
            distance: if top == Op::Phrase { distance } else { 0 },
            left: 0,
        });
    }
}

/// Items in the order they were read, each operator after its operands, turned to PostgreSQL's
/// order with each operator's `left` filled in, as `findoprnd` fills them.
fn laid_out(mut items: Vec<Item>) -> Vec<Item> {
    let mut sizes: Vec<usize> = Vec::new();
    for item in items.iter_mut() {
        match item {
            Item::Val { .. } => sizes.push(1),
            Item::Opr { op, left, .. } => {
                let right = sizes.pop().expect("an operator's operand");
                if *op == Op::Not {
                    *left = 1;
                    sizes.push(right + 1);
                } else {
                    let l = sizes.pop().expect("an operator's left operand");
                    *left = right + 1;
                    sizes.push(right + l + 1);
                }
            }
        }
    }
    items.reverse();
    items
}

/// A tsquery read as `tsqueryin` reads one (`parse_tsquery`, tsquery.c): its items. Where `held`,
/// an operand may start past MAXSTRPOS in the order it is read, as one an operator joined does.
fn parse_query(s: &str, held: bool) -> Result<Vec<Item>> {
    let s = cstring(s);
    let mut r = Reader {
        s,
        i: 0,
        wait: Wait::First,
        count: 0,
    };
    let mut out: Vec<Item> = Vec::new();
    let mut levels: Vec<Vec<(Op, u16)>> = vec![Vec::new()];
    let mut sumlen = 0;
    loop {
        match r.next()? {
            Tok::End => break,
            Tok::Val {
                word,
                weight,
                prefix,
            } => {
                if word.len() > LEXEME_MAX {
                    return Err(Error::new(format!("word is too long in tsquery: \"{s}\"")));
                }
                if !held && sumlen > STRING_MAX {
                    return Err(Error::new(format!("value is too big in tsquery: \"{s}\"")));
                }
                sumlen += word.len() + 1;
                out.push(Item::Val {
                    word,
                    weight,
                    prefix,
                });
            }
            Tok::Opr(op, distance) => {
                let stack = levels.last_mut().expect("a level");
                clean(stack, &mut out, op);
                if stack.len() >= STACK_DEPTH {
                    return Err(Error::new("tsquery is too complex"));
                }
                stack.push((op, distance));
            }
            Tok::Open => levels.push(Vec::new()),
            Tok::Close => {
                let mut stack = levels.pop().expect("a level");
                clean(&mut stack, &mut out, Op::Or);
            }
        }
    }
    let mut stack = levels.pop().expect("a level");
    clean(&mut stack, &mut out, Op::Or);
    if !out.is_empty()
        && (sumlen > ALLOC_MAX - HEADER || out.len() > (ALLOC_MAX - HEADER - sumlen) / ITEM)
    {
        return Err(Error::new("tsquery is too large"));
    }
    Ok(laid_out(out))
}

/// An operand written as `tsqueryout` writes one: quoted, then `:` and `*` for a prefix and the
/// letters of its weights.
fn operand_text(out: &mut String, word: &str, weight: u8, prefix: bool) {
    quoted(out, word);
    if weight != 0 || prefix {
        out.push(':');
        if prefix {
            out.push('*');
        }
        for (bit, letter) in [(8, 'A'), (4, 'B'), (2, 'C'), (1, 'D')] {
            if weight & bit != 0 {
                out.push(letter);
            }
        }
    }
}

/// A tsquery's items written as `tsqueryout` writes them (`infix`, tsquery.c), or where `held`, as
/// the snail holds them: with parentheses around the right operand of an operator that is the
/// same operator, and around a `!` past the operators one level of parentheses holds.
fn infix(items: &[Item], held: bool) -> String {
    enum Task {
        Node {
            i: usize,
            parent: i32,
            right: bool,
            pending: usize,
        },
        Text(String),
    }
    let mut out = String::new();
    if items.is_empty() {
        return out;
    }
    let mut tasks = vec![Task::Node {
        i: 0,
        parent: -1,
        right: false,
        pending: 0,
    }];
    while let Some(task) = tasks.pop() {
        let (i, parent, right, pending) = match task {
            Task::Text(t) => {
                out.push_str(&t);
                continue;
            }
            Task::Node {
                i,
                parent,
                right,
                pending,
            } => (i, parent, right, pending),
        };
        match &items[i] {
            Item::Val {
                word,
                weight,
                prefix,
            } => operand_text(&mut out, word, *weight, *prefix),
            Item::Opr { op: Op::Not, .. } => {
                let wrap = held && pending >= STACK_DEPTH;
                let pending = if wrap { 0 } else { pending };
                if wrap {
                    out.push_str("( ");
                    tasks.push(Task::Text(" )".into()));
                }
                out.push('!');
                tasks.push(Task::Node {
                    i: i + 1,
                    parent: Op::Not.priority(),
                    right: false,
                    pending: pending + 1,
                });
            }
            Item::Opr { op, distance, left } => {
                let p = op.priority();
                let wrap = p < parent || (right && p == parent && (held || *op == Op::Phrase));
                let pending = if wrap { 0 } else { pending };
                if wrap {
                    out.push_str("( ");
                    tasks.push(Task::Text(" )".into()));
                }
                tasks.push(Task::Node {
                    i: i + 1,
                    parent: p,
                    right: true,
                    pending: pending + 1,
                });
                tasks.push(Task::Text(match op {
                    Op::Or => " | ".into(),
                    Op::And => " & ".into(),
                    _ if *distance != 1 => format!(" <{distance}> "),
                    _ => " <-> ".into(),
                }));
                tasks.push(Task::Node {
                    i: i + left,
                    parent: p,
                    right: false,
                    pending,
                });
            }
        }
    }
    out
}

/// The items of a tsquery the snail holds.
fn held_query(held: &str) -> Vec<Item> {
    parse_query(held, true).expect("a tsquery the snail holds")
}

/// A tsquery read as `tsqueryin` reads one (tsquery.c), as the snail holds it: the text
/// `tsqueryout` writes, but with parentheses where that text would read back as another tree or
/// as none.
pub fn tsquery_in(s: &str) -> Result<String> {
    Ok(infix(&parse_query(s, false)?, true))
}

/// A tsquery the snail holds, written as `tsqueryout` writes it.
pub fn tsquery_out(held: &str) -> String {
    infix(&held_query(held), false)
}

fn query_size(items: &[Item]) -> usize {
    let operands: usize = items
        .iter()
        .map(|it| match it {
            Item::Val { word, .. } => word.len() + 1,
            Item::Opr { .. } => 0,
        })
        .sum();
    HEADER + ITEM * items.len() + operands
}

/// The bytes PostgreSQL lays a tsquery out in, its header among them: VARSIZE (COMPUTESIZE).
pub fn tsquery_size(held: &str) -> usize {
    query_size(&held_query(held))
}

/// pg_crc32_table (src/backend/utils/hash/pg_crc.c).
const CRC_TABLE: [u32; 256] = {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 {
                0xedb8_8320 ^ (c >> 1)
            } else {
                c >> 1
            };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
};

/// A lexeme's CRC as a tsquery's operand keeps it in `valcrc`: INIT_LEGACY_CRC32,
/// COMP_LEGACY_CRC32 and FIN_LEGACY_CRC32 (src/include/utils/pg_crc.h), read as a signed int32.
pub fn legacy_crc32(lexeme: &[u8]) -> i32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in lexeme {
        crc = CRC_TABLE[((crc >> 24) as u8 ^ b) as usize] ^ (crc << 8);
    }
    (crc ^ 0xffff_ffff) as i32
}

/// The key of a tsquery, ordered as `tsquery_cmp` orders them (`CompareTSQ`, tsquery_op.c, and
/// `QTNodeCompare`, tsquery_util.c), equal exactly where it finds them equal: its count of items
/// and its size in four bytes each, big-endian; then each item in PostgreSQL's order, an operator
/// as 0 and its code taken from 255, after its operands a phrase's distance taken from 0xFFFF in
/// two bytes, big-endian; an operand as 1, its `valcrc` with its sign bit flipped, taken from
/// 0xFFFFFFFF in four bytes, big-endian, and its bytes as a part of varying length. An operand's
/// weights and prefix flag are not in it.
pub fn tsquery_key(held: &str) -> Vec<u8> {
    let items = held_query(held);
    let mut out = Vec::with_capacity(8 + 8 * items.len());
    out.extend_from_slice(&(items.len() as u32).to_be_bytes());
    out.extend_from_slice(&(query_size(&items) as u32).to_be_bytes());
    // Each open operator: its operands still to come, and the distance written after them.
    let mut open: Vec<(usize, Option<u16>)> = Vec::new();
    for item in &items {
        match item {
            Item::Opr { op, distance, .. } => {
                out.push(0);
                out.push(255 - *op as u8);
                let arity = if *op == Op::Not { 1 } else { 2 };
                open.push((arity, (*op == Op::Phrase).then_some(*distance)));
                continue;
            }
            Item::Val { word, .. } => {
                out.push(1);
                let crc = legacy_crc32(word.as_bytes()) as u32 ^ 0x8000_0000;
                out.extend_from_slice(&(u32::MAX - crc).to_be_bytes());
                varying_part(&mut out, word.as_bytes());
            }
        }
        while let Some(top) = open.last_mut() {
            top.0 -= 1;
            if top.0 > 0 {
                break;
            }
            if let Some(d) = top.1 {
                out.extend_from_slice(&(u16::MAX - d).to_be_bytes());
            }
            open.pop();
        }
    }
    out
}

/// Items checked as `QTN2QT` (tsquery_util.c) lays them out, each operand's bytes after the
/// last's in the items' order.
fn packed(items: Vec<Item>) -> Result<Vec<Item>> {
    let mut at = 0;
    for item in &items {
        if let Item::Val { word, .. } = item {
            if at > STRING_MAX {
                return Err(Error::new("tsquery is too large"));
            }
            at += word.len() + 1;
        }
    }
    Ok(items)
}

/// Two tsqueries joined under a binary operator as `join_tsqueries` joins them: `a` its left
/// operand, `b` its right; either alone where the other is empty.
fn join(a: &str, b: &str, op: Op, distance: u16) -> Result<String> {
    let a = held_query(a);
    let b = held_query(b);
    if a.is_empty() {
        return Ok(infix(&b, true));
    }
    if b.is_empty() {
        return Ok(infix(&a, true));
    }
    let mut items = Vec::with_capacity(1 + a.len() + b.len());
    items.push(Item::Opr {
        op,
        distance,
        left: 1 + b.len(),
    });
    items.extend(b);
    items.extend(a);
    Ok(infix(&packed(items)?, true))
}

/// `a && b`: `tsquery_and` (tsquery_op.c), as the snail holds it.
pub fn tsquery_and(a: &str, b: &str) -> Result<String> {
    join(a, b, Op::And, 0)
}

/// `a || b`: `tsquery_or` (tsquery_op.c), as the snail holds it.
pub fn tsquery_or(a: &str, b: &str) -> Result<String> {
    join(a, b, Op::Or, 0)
}

/// `a <-> b`: `tsquery_phrase` (tsquery_op.c), as the snail holds it.
pub fn tsquery_phrase(a: &str, b: &str) -> Result<String> {
    join(a, b, Op::Phrase, 1)
}

/// `tsquery_phrase(a, b, distance)`: `tsquery_phrase_distance` (tsquery_op.c), as the snail holds
/// it.
pub fn tsquery_phrase_distance(a: &str, b: &str, distance: i32) -> Result<String> {
    if !(0..=DISTANCE_MAX as i32).contains(&distance) {
        return Err(Error::new(format!(
            "distance in phrase operator must be an integer value between zero and {DISTANCE_MAX} inclusive"
        )));
    }
    join(a, b, Op::Phrase, distance as u16)
}

/// `!! a`: `tsquery_not` (tsquery_op.c), as the snail holds it; the empty tsquery as it is.
pub fn tsquery_not(a: &str) -> Result<String> {
    let a = held_query(a);
    if a.is_empty() {
        return Ok(String::new());
    }
    let mut items = Vec::with_capacity(1 + a.len());
    items.push(Item::Opr {
        op: Op::Not,
        distance: 0,
        left: 1,
    });
    items.extend(a);
    Ok(infix(&packed(items)?, true))
}

/// `a @> b`: whether every lexeme of `b` is a lexeme of `a`, whatever the operators, weights and
/// prefixes: `tsq_mcontains` (tsquery_op.c). `a <@ b` is `tsq_mcontains(b, a)`.
pub fn tsq_mcontains(a: &str, b: &str) -> bool {
    let words = |q: &str| -> std::collections::BTreeSet<String> {
        held_query(q)
            .into_iter()
            .filter_map(|it| match it {
                Item::Val { word, .. } => Some(word),
                Item::Opr { .. } => None,
            })
            .collect()
    };
    words(b).is_subset(&words(a))
}

/// A match's answer that may not be known without positions: TSTernaryValue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Ternary {
    No,
    Yes,
    Maybe,
}

/// Where a phrase matched, as ExecPhraseData holds it: positions, each read through WEP_GETPOS;
/// whether they are where it does not match; and the match's width less one.
#[derive(Default)]
struct Found {
    pos: Vec<u16>,
    negate: bool,
    width: i32,
}

/// Whether a lexeme of the tsvector passes an operand's weights, its positions put in `data` where
/// asked for: `checkclass_str` (tsvector_op.c).
fn check_class(entry: &Entry, weight: u8, data: Option<&mut Found>) -> Ternary {
    if entry.pos.is_empty() {
        return if data.is_some() {
            Ternary::Maybe
        } else {
            Ternary::Yes
        };
    }
    let passes = |p: &u16| weight & (1 << weight_of(*p)) != 0;
    match data {
        Some(d) if weight != 0 => {
            d.pos = entry
                .pos
                .iter()
                .filter(|p| passes(p))
                .map(|&p| pos_of(p))
                .collect();
            if d.pos.is_empty() {
                Ternary::No
            } else {
                Ternary::Yes
            }
        }
        Some(d) => {
            d.pos = entry.pos.clone();
            Ternary::Yes
        }
        None if weight != 0 => {
            if entry.pos.iter().any(passes) {
                Ternary::Yes
            } else {
                Ternary::No
            }
        }
        None => Ternary::Yes,
    }
}

/// Whether the tsvector holds an operand, or for a prefix any lexeme it starts, its positions put
/// in `data` where asked for: `checkcondition_str` (tsvector_op.c).
fn check_condition(
    vector: &[Entry],
    word: &str,
    weight: u8,
    prefix: bool,
    mut data: Option<&mut Found>,
) -> Ternary {
    let found = vector.binary_search_by(|e| e.word.as_bytes().cmp(word.as_bytes()));
    let mut res = Ternary::No;
    if let Ok(k) = found {
        res = check_class(&vector[k], weight, data.as_deref_mut());
    }
    if prefix && (res != Ternary::Yes || data.is_some()) {
        let mut k = match found {
            Ok(k) | Err(k) => k,
        };
        if let Some(d) = data.as_deref_mut() {
            d.pos.clear();
        }
        res = Ternary::No;
        let mut all: Vec<u16> = Vec::new();
        while (res != Ternary::Yes || data.is_some())
            && k < vector.len()
            && vector[k].word.as_bytes().starts_with(word.as_bytes())
        {
            let sub = check_class(&vector[k], weight, data.as_deref_mut());
            if sub != Ternary::No {
                match data.as_deref_mut() {
                    Some(d) => {
                        if sub == Ternary::Maybe {
                            res = Ternary::Maybe;
                            all.clear();
                            break;
                        }
                        all.append(&mut d.pos);
                    }
                    None => {
                        if sub == Ternary::Yes || res == Ternary::No {
                            res = sub;
                        }
                    }
                }
            }
            k += 1;
        }
        if let Some(d) = data {
            if !all.is_empty() {
                all.sort_by_key(|&p| pos_of(p));
                all.dedup_by_key(|p| pos_of(*p));
                d.pos = all;
                res = Ternary::Yes;
            }
        }
    }
    res
}

const L_ONLY: u8 = 1;
const R_ONLY: u8 = 2;
const BOTH: u8 = 4;

/// Two operands' positions merged as `emit` asks, each moved by its offset: `TS_phrase_output`
/// (tsvector_op.c). Into `data` where it is given; else whether any would be.
fn phrase_output(
    data: Option<&mut Found>,
    l: &Found,
    r: &Found,
    emit: u8,
    loffset: i32,
    roffset: i32,
) -> Ternary {
    let (mut li, mut ri) = (0, 0);
    let mut out: Option<&mut Found> = data;
    while li < l.pos.len() || ri < r.pos.len() {
        let lpos = if li < l.pos.len() {
            (pos_of(l.pos[li]) as i32).wrapping_add(loffset)
        } else {
            if emit & R_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let rpos = if ri < r.pos.len() {
            (pos_of(r.pos[ri]) as i32).wrapping_add(roffset)
        } else {
            if emit & L_ONLY == 0 {
                break;
            }
            i32::MAX
        };
        let mut output = 0;
        match lpos.cmp(&rpos) {
            Ordering::Less => {
                if emit & L_ONLY != 0 {
                    output = lpos;
                }
                li += 1;
            }
            Ordering::Equal => {
                if emit & BOTH != 0 {
                    output = rpos;
                }
                li += 1;
                ri += 1;
            }
            Ordering::Greater => {
                if emit & R_ONLY != 0 {
                    output = rpos;
                }
                ri += 1;
            }
        }
        if output > 0 {
            match out.as_deref_mut() {
                Some(d) => d.pos.push(output as u16),
                None => return Ternary::Yes,
            }
        }
    }
    match out {
        Some(d) if !d.pos.is_empty() => Ternary::Yes,
        _ => Ternary::No,
    }
}

/// A tsquery's items from `i` matched at or below a phrase operator, where they matched put in
/// `data` where asked for: `TS_phrase_execute` (tsvector_op.c).
fn phrase_execute(items: &[Item], i: usize, vector: &[Entry], data: Option<&mut Found>) -> Ternary {
    let (op, distance, left) = match &items[i] {
        Item::Val {
            word,
            weight,
            prefix,
        } => return check_condition(vector, word, *weight, *prefix, data),
        Item::Opr { op, distance, left } => (*op, *distance as i32, *left),
    };
    match op {
        Op::Not => {
            let d = data.expect("a NOT under a phrase is asked where it matched");
            match phrase_execute(items, i + 1, vector, Some(&mut *d)) {
                Ternary::No => {
                    d.negate = true;
                    Ternary::Yes
                }
                Ternary::Yes if !d.pos.is_empty() => {
                    d.negate = !d.negate;
                    Ternary::Yes
                }
                Ternary::Yes if d.negate => {
                    d.negate = false;
                    Ternary::No
                }
                Ternary::Yes => Ternary::No,
                Ternary::Maybe => Ternary::Maybe,
            }
        }
        Op::Phrase | Op::And => {
            let mut l = Found::default();
            let mut r = Found::default();
            let lmatch = phrase_execute(items, i + left, vector, Some(&mut l));
            if lmatch == Ternary::No {
                return Ternary::No;
            }
            let rmatch = phrase_execute(items, i + 1, vector, Some(&mut r));
            if rmatch == Ternary::No {
                return Ternary::No;
            }
            if lmatch == Ternary::Maybe || rmatch == Ternary::Maybe {
                return Ternary::Maybe;
            }
            let mut data = data;
            let (loffset, roffset) = if op == Op::Phrase {
                if let Some(d) = data.as_deref_mut() {
                    d.width = distance + l.width + r.width;
                }
                (distance + r.width, 0)
            } else {
                let maxwidth = l.width.max(r.width);
                if let Some(d) = data.as_deref_mut() {
                    d.width = maxwidth;
                }
                (maxwidth - l.width, maxwidth - r.width)
            };
            if l.negate && r.negate {
                phrase_output(
                    data.as_deref_mut(),
                    &l,
                    &r,
                    BOTH | L_ONLY | R_ONLY,
                    loffset,
                    roffset,
                );
                if let Some(d) = data {
                    d.negate = true;
                }
                Ternary::Yes
            } else if l.negate {
                phrase_output(data, &l, &r, R_ONLY, loffset, roffset)
            } else if r.negate {
                phrase_output(data, &l, &r, L_ONLY, loffset, roffset)
            } else {
                phrase_output(data, &l, &r, BOTH, loffset, roffset)
            }
        }
        Op::Or => {
            let d = data.expect("an OR under a phrase is asked where it matched");
            let mut l = Found::default();
            let mut r = Found::default();
            let lmatch = phrase_execute(items, i + left, vector, Some(&mut l));
            let rmatch = phrase_execute(items, i + 1, vector, Some(&mut r));
            if lmatch == Ternary::No && rmatch == Ternary::No {
                return Ternary::No;
            }
            if lmatch == Ternary::Maybe || rmatch == Ternary::Maybe {
                return Ternary::Maybe;
            }
            if lmatch == Ternary::No {
                l.width = 0;
            }
            if rmatch == Ternary::No {
                r.width = 0;
            }
            let maxwidth = l.width.max(r.width);
            let (loffset, roffset) = (maxwidth - l.width, maxwidth - r.width);
            d.width = maxwidth;
            if l.negate && r.negate {
                phrase_output(Some(&mut *d), &l, &r, BOTH, loffset, roffset);
                d.negate = true;
                Ternary::Yes
            } else if l.negate {
                phrase_output(Some(&mut *d), &l, &r, L_ONLY, loffset, roffset);
                d.negate = true;
                Ternary::Yes
            } else if r.negate {
                phrase_output(Some(&mut *d), &l, &r, R_ONLY, loffset, roffset);
                d.negate = true;
                Ternary::Yes
            } else {
                phrase_output(Some(d), &l, &r, BOTH | L_ONLY | R_ONLY, loffset, roffset)
            }
        }
    }
}

/// A tsquery's items from `i` matched above any phrase operator: `TS_execute_recurse`
/// (tsvector_op.c) without its flags.
fn execute(items: &[Item], i: usize, vector: &[Entry]) -> Ternary {
    let (op, left) = match &items[i] {
        Item::Val {
            word,
            weight,
            prefix,
        } => return check_condition(vector, word, *weight, *prefix, None),
        Item::Opr { op, left, .. } => (*op, *left),
    };
    match op {
        Op::Not => match execute(items, i + 1, vector) {
            Ternary::No => Ternary::Yes,
            Ternary::Yes => Ternary::No,
            Ternary::Maybe => Ternary::Maybe,
        },
        Op::And => {
            let lmatch = execute(items, i + left, vector);
            if lmatch == Ternary::No {
                return Ternary::No;
            }
            match execute(items, i + 1, vector) {
                Ternary::No => Ternary::No,
                Ternary::Yes => lmatch,
                Ternary::Maybe => Ternary::Maybe,
            }
        }
        Op::Or => {
            let lmatch = execute(items, i + left, vector);
            if lmatch == Ternary::Yes {
                return Ternary::Yes;
            }
            match execute(items, i + 1, vector) {
                Ternary::No => lmatch,
                Ternary::Yes => Ternary::Yes,
                Ternary::Maybe => Ternary::Maybe,
            }
        }
        Op::Phrase => match phrase_execute(items, i, vector, None) {
            Ternary::Yes => Ternary::Yes,
            Ternary::No | Ternary::Maybe => Ternary::No,
        },
    }
}

/// The deepest a tsquery's items nest.
fn depth(items: &[Item]) -> usize {
    let mut deepest = 0;
    let mut open: Vec<usize> = Vec::new();
    for item in items {
        deepest = deepest.max(open.len() + 1);
        match item {
            Item::Opr { op, .. } => {
                open.push(if *op == Op::Not { 1 } else { 2 });
                continue;
            }
            Item::Val { .. } => {}
        }
        while let Some(top) = open.last_mut() {
            *top -= 1;
            if *top > 0 {
                break;
            }
            open.pop();
        }
    }
    deepest
}

/// The items a match walks at once on the caller's stack; a deeper tsquery is matched on a thread
/// of its own with a stack to fit it.
const SHALLOW: usize = 256;

/// `vector @@ query`: whether a tsvector matches a tsquery the snail holds, as `ts_match_vq`
/// (tsvector_op.c) matches them: `TS_execute` with no flags, a phrase that cannot be told
/// without positions not matching, and the empty tsquery matching nothing.
pub fn ts_match(vector: &str, query: &str) -> bool {
    let vector = held_vector(vector);
    let items = held_query(query);
    if items.is_empty() {
        return false;
    }
    let deep = depth(&items);
    if deep <= SHALLOW {
        return execute(&items, 0, &vector) != Ternary::No;
    }
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .stack_size((1 << 20) + deep * 4096)
            .spawn_scoped(s, || execute(&items, 0, &vector) != Ternary::No)
            .expect("a thread to match a deep tsquery on")
            .join()
            .expect("a match of a deep tsquery")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Every expected value below is PostgreSQL 19's, read on its server.

    #[test]
    fn a_tsvector_is_read_as_tsvectorin_reads_it_and_sized_as_it_lays_one_out() {
        for (input, want, size) in [
            ("1", "'1'", 13),
            ("1 ", "'1'", 13),
            (" 1", "'1'", 13),
            (" 1 ", "'1'", 13),
            ("1 2", "'1' '2'", 18),
            ("'1 2'", "'1 2'", 15),
            ("'1 \\'2'", "'1 ''2'", 16),
            ("'1 \\'2'3", "'1 ''2' '3'", 21),
            ("'1 \\'2' 3", "'1 ''2' '3'", 21),
            ("'1 \\'2' ' 3' 4 ", "' 3' '1 ''2' '4'", 27),
            (
                "'\\\\as' ab\\c ab\\\\c AB\\\\\\c ab\\\\\\\\c",
                "'AB\\\\c' '\\\\as' 'ab\\\\\\\\c' 'ab\\\\c' 'abc'",
                47,
            ),
            ("a:1,3 b:2", "'a':1,3 'b':2", 30),
            ("", "", 8),
            ("a", "'a'", 13),
            ("a:1", "'a':1", 18),
            ("b", "'b'", 13),
            ("ab", "'ab'", 14),
            ("a:1,2", "'a':1,2", 20),
            ("a:2", "'a':2", 18),
            ("a b", "'a' 'b'", 18),
            ("a:1A", "'a':1A", 18),
            ("b a a:3 a:1 b:2,2", "'a':1,3 'b':2", 30),
            ("a:1 a:1A", "'a':1A", 18),
            ("a:20000", "'a':16383", 18),
            ("ab b B é", "'B' 'ab' 'b' 'é'", 30),
            ("'it''s' \"x y\"", "'\"x' 'it''s' 'y\"'", 28),
            ("a:4294967297", "'a':1", 18),
            ("a:4294967291", "'a':16379", 18),
            ("a:99999999999999999999", "'a':16383", 18),
            ("a:1,20000,20000A", "'a':1,16383", 20),
            ("a:20000A,20000", "'a':16383A", 18),
            ("a:1A2 b:3DB", "'a':1A 'b':3B", 28),
            ("'a'b", "'a' 'b'", 18),
            ("'a''b'", "'a''b'", 15),
            ("a:3A b:2a", "'a':3A 'b':2A", 28),
            ("ba:1234 a:1B", "'a':1B 'ba':1234", 28),
            (
                "a b:89  ca:23A,64b d:34c",
                "'a' 'b':89 'ca':23A,64B 'd':34C",
                44,
            ),
            (
                "w:12B w:13* w:12,5,6 a:1,3* a:3 w asd:1dc asd",
                "'a':1,3A 'asd':1C 'w':5,6,12B,13A",
                48,
            ),
            ("a\tb\nc\u{b}d\u{c}e\rf", "'a' 'b' 'c' 'd' 'e' 'f'", 38),
            ("a:1 , b", "',' 'a':1 'b'", 27),
            ("a\u{a0}b", "'a\u{a0}b'", 16),
        ] {
            let text = tsvector_in(input).unwrap();
            assert_eq!(text, want, "{input:?}");
            assert_eq!(tsvector_size(&text), size, "{input:?}");
            assert_eq!(tsvector_in(&text).unwrap(), text, "{input:?}");
        }
        for (input, msg) in [
            ("'' '1' '2'", "syntax error in tsvector: \"'' '1' '2'\""),
            (
                "a:4294967296",
                "wrong position info in tsvector: \"a:4294967296\"",
            ),
            (
                "a:2147483648",
                "wrong position info in tsvector: \"a:2147483648\"",
            ),
            ("a:0", "wrong position info in tsvector: \"a:0\""),
            ("a:", "syntax error in tsvector: \"a:\""),
            ("a\\", "there is no escaped character: \"a\\\""),
            ("'abc", "syntax error in tsvector: \"'abc\""),
            ("a:1AB", "syntax error in tsvector: \"a:1AB\""),
            ("a:1,,2", "syntax error in tsvector: \"a:1,,2\""),
            ("a:1',b", "syntax error in tsvector: \"a:1',b\""),
        ] {
            assert_eq!(tsvector_in(input).unwrap_err().0, msg, "{input:?}");
        }
    }

    #[test]
    fn a_lexemes_positions_are_kept_as_postgres_sorts_them() {
        let weights = ["", "A", "B", "C"];
        let list = |n: usize, at: &dyn Fn(usize) -> usize| {
            (0..n)
                .map(|k| format!("{}{}", at(k), weights[k % 4]))
                .collect::<Vec<_>>()
                .join(",")
        };
        let s = format!("b:{}", list(60, &|k| 16380 + k % 9));
        assert_eq!(tsvector_in(&s).unwrap(), "'b':16380A,16381A,16382A,16383");
        let s = format!("a:{}", list(300, &|k| (k * 37) % 290 + 1));
        let text = tsvector_in(&s).unwrap();
        let want = "'a':1B,2C,3,4A,5B,6C,7B,8C,9,10A,11B,12C,13,14C,15,16A,17B,18C,19,20C,21,22A,23B,\
            24C,25,26C,27,28A,29B,30C,31,32C,33,34A,35B,36C,37,38A,39,40A,41B,42C,43,44A,45,46A,47B,48C,\
            49,50A,51,52A,53B,54C,55,56A,57,58A,59B,60C,61,62A,63,64A,65B,66C,67,68A,69,70A,71B,72C,73,\
            74A,75B,76A,77B,78C,79,80A,81B,82A,83B,84C,85,86A,87B,88A,89B,90C,91,92A,93B,94A,95B,96C,97,\
            98A,99B,100A,101B,102C,103,104A,105B,106A,107B,108C,109,110A,111B,112A,113B,114C,115,116A,\
            117B,118C,119B,120C,121,122A,123B,124C,125B,126C,127,128A,129B,130C,131B,132C,133,134A,135B,\
            136C,137B,138C,139,140A,141B,142C,143B,144C,145,146A,147B,148C,149B,150C,151,152A,153B,154C,\
            155,156C,157,158A,159B,160C,161,162C,163,164A,165B,166C,167,168C,169,170A,171B,172C,173,174C,\
            175,176A,177B,178C,179,180C,181,182A,183B,184C,185,186A,187,188A,189B,190C,191,192A,193,194A,\
            195B,196C,197,198A,199,200A,201B,202C,203,204A,205,206A,207B,208C,209,210A,211,212A,213B,214C,\
            215,216A,217,218A,219B,220C,221,222A,223B,224A,225B,226C,227,228A,229B,230A,231B,232C,233,\
            234A,235B,236A,237B,238C,239,240A,241B,242A,243B,244C,245,246A,247B,248A,249B,250C,251,252A,\
            253B,254A,255B,256C";
        assert_eq!(text, want);
        assert_eq!(tsvector_size(&text), 528);
    }

    #[test]
    fn a_tsvector_past_its_bounds_is_refused() {
        let words = |n: usize| {
            (1..=n)
                .map(|i| format!("{i:x>2047}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        assert_eq!(tsvector_in(&words(512)).unwrap().len(), 1_049_599);
        for n in [513, 514] {
            assert_eq!(
                tsvector_in(&words(n)).unwrap_err().0,
                "string is too long for tsvector (1050111 bytes, max 1048575 bytes)"
            );
        }
        assert_eq!(
            tsvector_in(&"q".repeat(2048)).unwrap_err().0,
            "word is too long (2048 bytes, max 2047 bytes)"
        );
        assert_eq!(tsvector_in(&"q".repeat(2047)).unwrap().len(), 2049);
    }

    /// Values listed in PostgreSQL's order with their ranks: equal ranks are equal values.
    fn assert_ordered(keys: &[(i64, Vec<u8>, &str)]) {
        for a in keys {
            for b in keys {
                assert_eq!(a.1.cmp(&b.1), a.0.cmp(&b.0), "{:?} {:?}", a.2, b.2);
            }
        }
    }

    #[test]
    fn a_tsvector_is_keyed_as_tsvector_cmp_orders_them() {
        let ranked = [
            (1, ""),
            (2, "a"),
            (3, "b"),
            (4, "aa"),
            (5, "ab"),
            (6, "a:16383"),
            (7, "a:2"),
            (8, "a:1A"),
            (8, "a:1 a:1A"),
            (9, "a:1B"),
            (10, "a:1C"),
            (11, "a:1"),
            (11, "a:1D"),
            (12, "ab:1"),
            (13, "a b"),
            (14, "a:1,2"),
            (15, "a:1,2,3"),
            (16, "'it''s' \"x y\""),
            (17, "a:1,3 b:2"),
            (17, "b a a:3 a:1 b:2,2"),
            (18, "ab b B é"),
        ];
        let keys: Vec<_> = ranked
            .iter()
            .map(|&(r, v)| (r, tsvector_key(&tsvector_in(v).unwrap()), v))
            .collect();
        assert_ordered(&keys);
    }

    #[test]
    fn a_tsquery_is_held_as_text_that_reads_back_and_printed_as_tsqueryout_prints_it() {
        for (input, out, held, size) in [
            ("1", "'1'", "'1'", 22),
            (" 1 ", "'1'", "'1'", 22),
            ("'1 2'", "'1 2'", "'1 2'", 24),
            ("'1 \\'2'", "'1 ''2'", "'1 ''2'", 25),
            ("!1", "!'1'", "!'1'", 34),
            ("1|!2", "'1' | !'2'", "'1' | !'2'", 60),
            ("!(!1|!2)", "!( !'1' | !'2' )", "!( !'1' | !'2' )", 84),
            ("!(1|2)", "!( '1' | '2' )", "!( '1' | '2' )", 60),
            ("!1&!2", "!'1' & !'2'", "!'1' & !'2'", 72),
            ("1&(2)", "'1' & '2'", "'1' & '2'", 48),
            ("!(1)&2", "!'1' & '2'", "!'1' & '2'", 60),
            ("1|(2&3)", "'1' | '2' & '3'", "'1' | '2' & '3'", 74),
            ("(1|2)&3", "( '1' | '2' ) & '3'", "( '1' | '2' ) & '3'", 74),
            (
                "!(1|2)&3",
                "!( '1' | '2' ) & '3'",
                "!( '1' | '2' ) & '3'",
                86,
            ),
            (
                "(!1|2)&3",
                "( !'1' | '2' ) & '3'",
                "( !'1' | '2' ) & '3'",
                86,
            ),
            (
                "1|(2|(4|(5|6)))",
                "'1' | '2' | '4' | '5' | '6'",
                "'1' | ( '2' | ( '4' | ( '5' | '6' ) ) )",
                126,
            ),
            (
                "1|2|4|5|6",
                "'1' | '2' | '4' | '5' | '6'",
                "'1' | '2' | '4' | '5' | '6'",
                126,
            ),
            (
                "1&(2&(4&(5|!6)))",
                "'1' & '2' & '4' & ( '5' | !'6' )",
                "'1' & ( '2' & ( '4' & ( '5' | !'6' ) ) )",
                138,
            ),
            (
                "1&('2'&(' 4'&(\\|5 | '6 \\' !|&')))",
                "'1' & '2' & ' 4' & ( '|5' | '6 '' !|&' )",
                "'1' & ( '2' & ( ' 4' & ( '|5' | '6 '' !|&' ) ) )",
                134,
            ),
            ("'\\\\as'", "'\\\\as'", "'\\\\as'", 24),
            (
                "a:* & nbb:*ac | doo:a* | goo",
                "'a':* & 'nbb':*AC | 'doo':*A | 'goo'",
                "'a':* & 'nbb':*AC | 'doo':*A | 'goo'",
                106,
            ),
            ("!!!b", "!!!'b'", "!!!'b'", 58),
            ("!(!b)", "!!'b'", "!!'b'", 46),
            ("!!a & !!b", "!!'a' & !!'b'", "!!'a' & !!'b'", 96),
            ("a & b", "'a' & 'b'", "'a' & 'b'", 48),
            ("aa", "'aa'", "'aa'", 23),
            ("a <-> b", "'a' <-> 'b'", "'a' <-> 'b'", 48),
            ("a:*", "'a':*", "'a':*", 22),
            ("a:A", "'a':A", "'a':A", 22),
            ("a <2> b", "'a' <2> 'b'", "'a' <2> 'b'", 48),
            ("a & (b & c)", "'a' & 'b' & 'c'", "'a' & ( 'b' & 'c' )", 74),
            ("a:AB*", "'a':*AB", "'a':*AB", 22),
            ("", "", "", 8),
            ("   ", "", "", 8),
            (
                "a <-> (b <-> c)",
                "'a' <-> ( 'b' <-> 'c' )",
                "'a' <-> ( 'b' <-> 'c' )",
                74,
            ),
            (
                "(a <-> b) <-> c",
                "'a' <-> 'b' <-> 'c'",
                "'a' <-> 'b' <-> 'c'",
                74,
            ),
            ("a <0> b", "'a' <0> 'b'", "'a' <0> 'b'", 48),
            ("a <16384> b", "'a' <16384> 'b'", "'a' <16384> 'b'", 48),
            ("a<->b", "'a' <-> 'b'", "'a' <-> 'b'", 48),
            (
                "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!a",
                "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!'a'",
                "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!'a'",
                406,
            ),
            (
                "a & (b | c) & (d & e)",
                "'a' & ( 'b' | 'c' ) & 'd' & 'e'",
                "'a' & ( 'b' | 'c' ) & ( 'd' & 'e' )",
                126,
            ),
            (
                "(a | b) | (c | d)",
                "'a' | 'b' | 'c' | 'd'",
                "'a' | 'b' | ( 'c' | 'd' )",
                100,
            ),
        ] {
            let h = tsquery_in(input).unwrap();
            assert_eq!(h, held, "{input:?}");
            assert_eq!(tsquery_out(&h), out, "{input:?}");
            assert_eq!(tsquery_size(&h), size, "{input:?}");
            assert_eq!(
                held_query(&h),
                parse_query(input, false).unwrap(),
                "{input:?}"
            );
        }
        for (input, msg) in [
            (
                "a <16385> b",
                "distance in phrase operator must be an integer value between zero and 16384 inclusive",
            ),
            (
                "a <99999999999999999999> b",
                "distance in phrase operator must be an integer value between zero and 16384 inclusive",
            ),
            ("a <->", "syntax error in tsquery: \"a <->\""),
            ("a <-> ", "no operand in tsquery: \"a <-> \""),
            ("()", "syntax error in tsquery: \"()\""),
            ("a)", "syntax error in tsquery: \"a)\""),
            ("(a", "syntax error in tsquery: \"(a\""),
            ("!", "no operand in tsquery: \"!\""),
            (":a", "syntax error in tsquery: \":a\""),
            ("a:x", "syntax error in tsquery: \"a:x\""),
            ("a & ", "no operand in tsquery: \"a & \""),
            ("!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!a", "tsquery is too complex"),
            ("a | b & c <-> !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!d", "tsquery is too complex"),
            ("'a'b", "syntax error in tsquery: \"'a'b\""),
            ("\\", "there is no escaped character: \"\\\""),
        ] {
            assert_eq!(tsquery_in(input).unwrap_err().0, msg, "{input:?}");
        }
        assert!(tsquery_in("a | b & c <-> !!!!!!!!!!!!!!!!!!!!!!!!!!!!!d").is_ok());
    }

    #[test]
    fn a_tsquery_past_its_bounds_is_refused() {
        let operands = |n: usize| {
            (1..=n)
                .map(|i| format!("{i:x>2047}"))
                .collect::<Vec<_>>()
                .join(" | ")
        };
        let q = tsquery_in(&operands(512)).unwrap();
        assert_eq!(tsquery_out(&q).len(), 1_050_621);
        let msg = tsquery_in(&operands(513)).unwrap_err().0;
        assert!(
            msg.starts_with("value is too big in tsquery: \"xxx"),
            "{}",
            &msg[..40]
        );
        assert_eq!(
            tsquery_in(&"q".repeat(2048)).unwrap_err().0,
            format!("word is too long in tsquery: \"{}\"", "q".repeat(2048))
        );
        assert_eq!(tsquery_size(&tsquery_and(&q, "x").unwrap()), 1_060_886);
        // Its operands, read left to right, start past MAXSTRPOS: no text reads it back.
        let joined = tsquery_and(&q, "x").unwrap();
        assert!(tsquery_in(&tsquery_out(&joined))
            .unwrap_err()
            .0
            .starts_with("value is too big"));
        assert!(tsquery_in(&joined)
            .unwrap_err()
            .0
            .starts_with("value is too big"));
        assert_eq!(tsquery_size(&tsquery_not(&q).unwrap()), 1_060_872);
        assert_eq!(tsquery_size(&tsquery_phrase(&q, "x").unwrap()), 1_060_886);
        for r in [
            tsquery_and("x", &q),
            tsquery_and(&q, &q),
            tsquery_phrase_distance("x", &q, 3),
        ] {
            assert_eq!(r.unwrap_err().0, "tsquery is too large");
        }
    }

    #[test]
    fn a_tsquery_is_keyed_as_tsquery_cmp_orders_them() {
        let ranked = [
            (1, ""),
            (2, "a"),
            (2, "a:*"),
            (2, "a:A"),
            (2, "a:AB*"),
            (2, "a:B"),
            (3, "c"),
            (4, "b"),
            (5, "aa"),
            (6, "ab"),
            (7, "ba"),
            (8, "!a"),
            (9, "!b"),
            (10, "!!a"),
            (11, "a <3> b"),
            (12, "a <2> b"),
            (13, "a <-> b"),
            (14, "a | b"),
            (15, "b & a"),
            (16, "a & b"),
            (17, "a <-> (b <-> c)"),
            (18, "(a <-> b) <-> c"),
            (19, "a & (b & c)"),
            (20, "(a | b) & c"),
            (21, "(a & b) & c"),
        ];
        let keys: Vec<_> = ranked
            .iter()
            .map(|&(r, q)| (r, tsquery_key(&tsquery_in(q).unwrap()), q))
            .collect();
        assert_ordered(&keys);
    }

    #[test]
    fn a_lexemes_crc_is_postgres_legacy_crc32() {
        assert_eq!(CRC_TABLE[1], 0x7707_3096);
        assert_eq!(CRC_TABLE[255], 0x2d02_ef8d);
        // 'a' < 'c' < 'b' as tsqueries: the larger CRC first.
        assert!(legacy_crc32(b"a") > legacy_crc32(b"c"));
        assert!(legacy_crc32(b"c") > legacy_crc32(b"b"));
        assert_eq!(legacy_crc32(b""), 0);
    }

    #[test]
    fn tsquery_operators_join_as_postgres_joins_them() {
        let q = |s: &str| tsquery_in(s).unwrap();
        for (got, out) in [
            (
                tsquery_and(&q("foo & bar"), &q("asd")),
                "'foo' & 'bar' & 'asd'",
            ),
            (
                tsquery_or(&q("foo & bar"), &q("asd & fg")),
                "'foo' & 'bar' | 'asd' & 'fg'",
            ),
            (
                tsquery_or(&q("foo & bar"), &tsquery_not(&q("asd & fg")).unwrap()),
                "'foo' & 'bar' | !( 'asd' & 'fg' )",
            ),
            (
                tsquery_and(&q("foo & bar"), &q("asd | fg")),
                "'foo' & 'bar' & ( 'asd' | 'fg' )",
            ),
            (
                tsquery_phrase(&q("a"), &q("b & d")),
                "'a' <-> ( 'b' & 'd' )",
            ),
            (
                tsquery_phrase(&q("a & g"), &q("b & d")),
                "( 'a' & 'g' ) <-> ( 'b' & 'd' )",
            ),
            (
                tsquery_phrase(&q("a & g"), &q("b | d")),
                "( 'a' & 'g' ) <-> ( 'b' | 'd' )",
            ),
            (
                tsquery_phrase(&q("a & g"), &q("b <-> d")),
                "( 'a' & 'g' ) <-> ( 'b' <-> 'd' )",
            ),
            (
                tsquery_phrase_distance(&q("a <3> g"), &q("b & d"), 10),
                "'a' <3> 'g' <10> ( 'b' & 'd' )",
            ),
            (tsquery_and(&q("a"), &q("b & c")), "'a' & 'b' & 'c'"),
            (
                tsquery_or(&q("a & b"), &q("c | d")),
                "'a' & 'b' | 'c' | 'd'",
            ),
            (tsquery_and("", &q("a")), "'a'"),
            (tsquery_or(&q("a"), ""), "'a'"),
            (tsquery_not(""), ""),
            (tsquery_phrase(&q("a"), ""), "'a'"),
        ] {
            assert_eq!(tsquery_out(&got.unwrap()), out);
        }
        let held = tsquery_and(&q("a"), &q("b & c")).unwrap();
        assert_eq!(held, "'a' & ( 'b' & 'c' )");
        assert_eq!(
            tsquery_size(&tsquery_and(&q("foo & bar"), &q("asd")).unwrap()),
            80
        );
        for d in [-1, 16385] {
            assert_eq!(
                tsquery_phrase_distance("", "", d).unwrap_err().0,
                "distance in phrase operator must be an integer value between zero and 16384 inclusive"
            );
        }
        // A run of !! past what one level of parentheses holds, under | & <->.
        let mut w = q("w");
        for _ in 0..31 {
            w = tsquery_not(&w).unwrap();
        }
        let held = tsquery_or(
            &q("x"),
            &tsquery_and(&q("y"), &tsquery_phrase(&q("z"), &w).unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            held,
            "'x' | 'y' & 'z' <-> !!!!!!!!!!!!!!!!!!!!!!!!!!!!!( !!'w' )"
        );
        assert_eq!(
            tsquery_out(&held),
            "'x' | 'y' & 'z' <-> !!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!'w'"
        );
        assert!(tsquery_in(&tsquery_out(&held)).is_err());
    }

    #[test]
    fn tsq_mcontains_compares_lexemes_alone() {
        let q = |s: &str| tsquery_in(s).unwrap();
        for (a, b, want) in [
            ("a & b", "b", true),
            ("b", "a & b", false),
            ("a:* | !b", "b <-> a:A", true),
            ("a", "", true),
            ("", "a", false),
            ("ab", "a", false),
        ] {
            assert_eq!(tsq_mcontains(&q(a), &q(b)), want, "{a:?} @> {b:?}");
        }
    }

    #[test]
    fn tsvectors_concatenate_as_tsvector_concat_joins_them() {
        let v = |s: &str| tsvector_in(s).unwrap();
        for (a, b, want) in [
            ("a:3A b:2a", "ba:1234 a:1B", "'a':3A,4B 'b':2A 'ba':1237"),
            ("a:16383 b:1", "a:5 c:2", "'a':16383 'b':1 'c':16383"),
            ("a:1", "a", "'a':1"),
            ("a", "a:1", "'a':1"),
            ("b:16382A x", "b:3 x:2B", "'b':16382A,16383 'x':16383B"),
            ("a:16383", "a:1", "'a':16383"),
            ("", "a:2", "'a':2"),
            ("a b", "c:4", "'a' 'b' 'c':4"),
        ] {
            assert_eq!(
                tsvector_concat(&v(a), &v(b)).unwrap(),
                want,
                "{a:?} || {b:?}"
            );
        }
        let words = |n: usize, y: bool| {
            v(&(1..=n)
                .map(|i| {
                    if y {
                        format!("{i:y>2047}")
                    } else {
                        format!("{i:x>2047}")
                    }
                })
                .collect::<Vec<_>>()
                .join(" "))
        };
        let (q, r) = (words(256, false), words(257, true));
        assert_eq!(tsvector_concat(&q, &q).unwrap().len(), 524_799);
        assert_eq!(
            tsvector_concat(&q, &r).unwrap_err().0,
            "string is too long for tsvector (1050111 bytes, max 1048575 bytes)"
        );
    }

    #[test]
    fn a_tsvector_matches_a_tsquery_as_ts_match_vq_matches_them() {
        for (v, q, want) in [
            ("'a' 'b':89 'ca':23A,64B 'd':34C", "'d':AC & 'ca'", true),
            ("'a' 'b':89 'ca':23A,64B 'd':34C", "'d':AC & 'ca':B", true),
            ("'a' 'b':89 'ca':23A,64B 'd':34C", "'d':AC & 'ca':C", false),
            ("'a' 'b':89 'ca':23A,64B 'd':34C", "'d':AC & 'c':*C", false),
            ("'a' 'b':89 'ca':23A,64B 'd':34C", "'d':AC & 'c':*BC", true),
            (
                "'a' 'b':89 'ca':23A,64B 'cb':80C 'd':34C",
                "'d':AC & 'c':*C",
                true,
            ),
            ("'wa':1 'wb':2A", "'w':*D <-> 'w':*A", true),
            ("'wa':1A 'wb':2", "'w':*D <-> 'w':*A", false),
            ("'wa':1A", "'w':*D", false),
            ("'wa':1A", "!'w':*D", true),
            ("'wa'", "'w':*D", true),
            ("'wa'", "!'w':*A", false),
            ("'supeanova' 'supernova'", "'super'", false),
            ("'supernova' 'supeznova'", "'super':*", true),
            ("'1':1,4 '2':2 '3':3", "'1' <2> '3'", true),
            ("'1':1,3 '2':2,4", "'1' <3> '2'", true),
            ("'1':1 '11':3 '2':2 '3':4", "'1':* <-> '3'", true),
            ("'1':1 '2':2 '3':3 '4':4", "'1' <2> ( '2' <-> '3' )", false),
            ("'1' '2' '3' '4'", "'1' <-> '2' <-> '3'", false),
            ("'q':1,3 'x':2 'y':4", "'q' <-> ( 'x' & 'y' )", false),
            ("'q':1 'y':2 'z':3", "'q' <-> ( 'x' | 'y' <-> 'z' )", true),
            ("'q':1 'x':3 'y':2", "'q' <-> ( 'x' | 'y' <-> 'z' )", false),
            ("'q':3 'y':1,2", "( 'x' | 'y' <-> 'z' ) <-> 'q'", false),
            ("'q':3 'y':1,2", "( !'x' | 'y' <-> 'z' ) <-> 'q'", true),
            ("'q':3 'y':1,2", "( 'x' | 'y' <-> !'z' ) <-> 'q'", true),
            ("'q':2 'x':1", "( !'x' | 'y' <-> 'z' ) <-> 'q'", false),
            ("'q':3 'x':1 'y':2", "( !'x' | 'y' ) <-> 'y' <-> 'q'", false),
            ("'q':3 'x':1 'y':2", "( !'x' | !'y' ) <-> 'y' <-> 'q'", true),
            ("'q':3 'x':1 'y':2", "( 'x' | !!'z' ) <-> 'y' <-> 'q'", true),
            ("'q':3 'x':1 'y':2,4", "!'x' <-> !!'y'", true),
            ("'q':3 'x':1 'y':2,4", "!( 'x' <-> 'y' )", false),
            ("'q' 'x' 'y'", "!'x' <-> !'y'", false),
            ("'q' 'x' 'y'", "!( 'x' <-> 'y' )", true),
            ("", "!'foo'", true),
            ("'a':1 'b':2", "'a' <0> 'b'", false),
            ("'a':1 'b':3", "'a' <2> 'b'", true),
            ("'a':1 'b':3", "'a' <0> 'a':*", true),
            ("'a':1", "", false),
        ] {
            let q = tsquery_in(q).unwrap();
            assert_eq!(
                ts_match(&tsvector_in(v).unwrap(), &q),
                want,
                "{v:?} @@ {q:?}"
            );
        }
    }

    #[test]
    fn a_deep_tsquery_is_read_written_keyed_and_matched() {
        let n = 20_000;
        let chain = vec!["a"; n].join(" & ");
        let held = tsquery_in(&chain).unwrap();
        assert_eq!(tsquery_out(&held).len(), 6 * n - 3);
        assert_eq!(tsquery_size(&held), HEADER + ITEM * (2 * n - 1) + 2 * n);
        assert_eq!(tsquery_key(&held).len(), 8 + 2 * (n - 1) + n * 8);
        assert!(ts_match("'a':1", &held));
        assert!(!ts_match("'b'", &held));
        let nested = format!(
            "{}'a' & 'a'{}",
            "'a' & ( ".repeat(n - 1),
            " )".repeat(n - 1)
        );
        let held = tsquery_in(&nested).unwrap();
        assert_eq!(held, nested);
        assert!(ts_match("'a'", &held));
        let phrase = vec!["a"; n].join(" <-> ");
        let held = tsquery_in(&phrase).unwrap();
        assert!(!ts_match("'a':1,2,3", &held));
    }
}
