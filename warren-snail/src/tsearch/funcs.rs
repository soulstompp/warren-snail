//! Text search's functions as PostgreSQL computes them: those of a tsvector (`tsvector_op.c`),
//! of a tsquery (`tsquery_op.c`, `tsquery_cleanup.c`, `tsquery_rewrite.c`, `tsquery_util.c`), and
//! the ranks of `tsrank.c`, each on a tsvector or tsquery as `tsearch` holds it, refusing what
//! PostgreSQL refuses with its message.

use super::{
    depth, execute, held_query, held_vector, infix, legacy_crc32, on_stack, packed, pg_qsort,
    pos_of, vector_text, weight_of, Entry, Found, Item, Op, Ternary, POSITION_MAX,
};
use crate::{Error, Result};
use std::cmp::Ordering;

/// A weight's code from its letter, 3 for A down to 0 for D: `parse_weight` (tsvector_op.c).
fn parse_weight(c: u8) -> Result<u16> {
    match c {
        b'A' | b'a' => Ok(3),
        b'B' | b'b' => Ok(2),
        b'C' | b'c' => Ok(1),
        b'D' | b'd' => Ok(0),
        b' '..=0x7e => Err(Error::new(format!(
            "unrecognized weight: \"{}\"",
            c as char
        ))),
        _ => Err(Error::new(format!("unrecognized weight: \"\\{c:03o}\""))),
    }
}

/// The index of a lexeme among a tsvector's, by its bytes: `tsvector_bsearch` (tsvector_op.c).
fn find(entries: &[Entry], lexeme: &str) -> Option<usize> {
    entries
        .binary_search_by(|e| e.word.as_bytes().cmp(lexeme.as_bytes()))
        .ok()
}

/// `length(tsvector)`: its count of lexemes (`tsvector_length`).
pub fn ts_length(vector: &str) -> i32 {
    held_vector(vector).len() as i32
}

/// `strip(tsvector)`: its lexemes without their positions (`tsvector_strip`).
pub fn strip(vector: &str) -> String {
    let mut entries = held_vector(vector);
    for e in &mut entries {
        e.pos.clear();
    }
    vector_text(&entries)
}

/// `setweight(tsvector, "char")`: every position given the weight (`tsvector_setweight`).
pub fn setweight(vector: &str, weight: u8) -> Result<String> {
    let w = parse_weight(weight)?;
    let mut entries = held_vector(vector);
    for e in &mut entries {
        for p in &mut e.pos {
            *p = w << 14 | pos_of(*p);
        }
    }
    Ok(vector_text(&entries))
}

/// `setweight(tsvector, "char", text[])`: the positions of each lexeme listed given the weight, a
/// NULL among them passed over (`tsvector_setweight_by_filter`).
pub fn setweight_lexemes(vector: &str, weight: u8, lexemes: &[Option<String>]) -> Result<String> {
    let w = parse_weight(weight)?;
    let mut entries = held_vector(vector);
    for lexeme in lexemes.iter().flatten() {
        if let Some(k) = find(&entries, lexeme) {
            for p in &mut entries[k].pos {
                *p = w << 14 | pos_of(*p);
            }
        }
    }
    Ok(vector_text(&entries))
}

/// `ts_delete(tsvector, text)`: the tsvector without the lexeme (`tsvector_delete_str`).
pub fn ts_delete(vector: &str, lexeme: &str) -> String {
    ts_delete_lexemes(vector, &[Some(lexeme.to_string())])
}

/// `ts_delete(tsvector, text[])`: the tsvector without each lexeme listed, a NULL among them
/// passed over (`tsvector_delete_arr`).
pub fn ts_delete_lexemes(vector: &str, lexemes: &[Option<String>]) -> String {
    let mut entries = held_vector(vector);
    let mut gone = vec![false; entries.len()];
    for lexeme in lexemes.iter().flatten() {
        if let Some(k) = find(&entries, lexeme) {
            gone[k] = true;
        }
    }
    let mut k = 0;
    entries.retain(|_| {
        k += 1;
        !gone[k - 1]
    });
    vector_text(&entries)
}

/// `ts_filter(tsvector, "char"[])`: the lexemes with positions of the weights given, each with
/// those positions alone (`tsvector_filter`).
pub fn ts_filter(vector: &str, weights: &[Option<u8>]) -> Result<String> {
    let mut mask = 0u8;
    for w in weights {
        let Some(w) = w else {
            return Err(Error::new("weight array may not contain nulls"));
        };
        mask |= 1 << parse_weight(*w)?;
    }
    let mut out = Vec::new();
    for mut e in held_vector(vector) {
        e.pos.retain(|&p| mask & (1 << weight_of(p)) != 0);
        if !e.pos.is_empty() {
            out.push(e);
        }
    }
    Ok(vector_text(&out))
}

/// `tsvector_to_array(tsvector)`: its lexemes in order (`tsvector_to_array`).
pub fn tsvector_to_array(vector: &str) -> Vec<String> {
    held_vector(vector).into_iter().map(|e| e.word).collect()
}

/// `array_to_tsvector(text[])`: the lexemes listed, sorted and made unique, without positions
/// (`array_to_tsvector`).
pub fn array_to_tsvector(lexemes: &[Option<String>]) -> Result<String> {
    let mut entries = Vec::with_capacity(lexemes.len());
    for lexeme in lexemes {
        let Some(word) = lexeme else {
            return Err(Error::new("lexeme array may not contain nulls"));
        };
        if word.is_empty() {
            return Err(Error::new("lexeme array may not contain empty strings"));
        }
        if word.len() > super::LEXEME_MAX {
            return Err(Error::new(format!(
                "word is too long ({} bytes, max {} bytes)",
                word.len(),
                super::LEXEME_MAX
            )));
        }
        entries.push(Entry {
            word: word.clone(),
            pos: Vec::new(),
        });
    }
    entries.sort_by(|a, b| a.word.as_bytes().cmp(b.word.as_bytes()));
    entries.dedup_by(|a, b| a.word == b.word);
    let n: usize = entries.iter().map(|e| e.word.len()).sum();
    if n > super::STRING_MAX {
        return Err(super::too_long(n));
    }
    Ok(vector_text(&entries))
}

/// A row of `unnest(tsvector)`: a lexeme, its positions and their weights' letters, both NULL
/// where it has no positions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unnested {
    pub lexeme: String,
    pub positions: Option<Vec<i16>>,
    pub weights: Option<Vec<String>>,
}

/// `unnest(tsvector)`: a row for each lexeme, in order (`tsvector_unnest`).
pub fn unnest(vector: &str) -> Vec<Unnested> {
    held_vector(vector)
        .into_iter()
        .map(|e| {
            let has = !e.pos.is_empty();
            Unnested {
                positions: has.then(|| e.pos.iter().map(|&p| pos_of(p) as i16).collect()),
                weights: has.then(|| {
                    e.pos
                        .iter()
                        .map(|&p| char::from(b'D' - weight_of(p) as u8).to_string())
                        .collect()
                }),
                lexeme: e.word,
            }
        })
        .collect()
}

/// `numnode(tsquery)`: its count of operands and operators (`tsquery_numnode`).
pub fn numnode(query: &str) -> i32 {
    held_query(query).len() as i32
}

/// A tsquery's node as `QTNode` (tsquery_util.c) holds one: an operand, a stop word's place, or an
/// operator and its operands, its right operand first; the bits of its operands' CRCs, and whether
/// a rewrite put it in.
#[derive(Clone, Debug)]
pub(super) struct Node {
    pub(super) kind: Kind,
    pub(super) child: Vec<Node>,
    sign: u32,
    nochange: bool,
}

/// What a node is, as its QueryItem's type and contents say.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Kind {
    Val {
        word: String,
        weight: u8,
        prefix: bool,
    },
    Stop,
    Opr {
        op: Op,
        distance: u16,
    },
}

impl Kind {
    /// QI_VAL, QI_OPR, QI_VALSTOP.
    fn code(&self) -> u8 {
        match self {
            Kind::Val { .. } => 1,
            Kind::Opr { .. } => 2,
            Kind::Stop => 3,
        }
    }
}

impl Node {
    pub(super) fn val(word: String, weight: u8, prefix: bool) -> Node {
        let sign = 1u32 << (legacy_crc32(word.as_bytes()) as u32 % 32);
        Node {
            kind: Kind::Val {
                word,
                weight,
                prefix,
            },
            child: Vec::new(),
            sign,
            nochange: false,
        }
    }

    pub(super) fn stop() -> Node {
        Node {
            kind: Kind::Stop,
            child: Vec::new(),
            sign: 0,
            nochange: false,
        }
    }

    /// An operator over its operands, the right one first.
    pub(super) fn opr(op: Op, distance: u16, child: Vec<Node>) -> Node {
        let sign = child.iter().fold(0, |s, c| s | c.sign);
        Node {
            kind: Kind::Opr { op, distance },
            child,
            sign,
            nochange: false,
        }
    }

    fn op(&self) -> Option<Op> {
        match self.kind {
            Kind::Opr { op, .. } => Some(op),
            _ => None,
        }
    }
}

/// The tree of a tsquery's items from `i`: `QT2QTN` (tsquery_util.c).
pub(super) fn tree(items: &[Item], i: usize) -> Node {
    match &items[i] {
        Item::Val {
            word,
            weight,
            prefix,
        } => Node::val(word.clone(), *weight, *prefix),
        Item::Opr { op, distance, left } => {
            let mut child = vec![tree(items, i + 1)];
            if *op != Op::Not {
                child.push(tree(items, i + left));
            }
            Node::opr(*op, *distance, child)
        }
    }
}

/// A tree of binary operators laid out as items: `QTN2QT` (tsquery_util.c), each operator before
/// its right operand's items, then its left's.
pub(super) fn flat(node: &Node, out: &mut Vec<Item>) {
    match &node.kind {
        Kind::Val {
            word,
            weight,
            prefix,
        } => out.push(Item::Val {
            word: word.clone(),
            weight: *weight,
            prefix: *prefix,
        }),
        Kind::Stop => {}
        Kind::Opr { op, distance } => {
            let at = out.len();
            out.push(Item::Opr {
                op: *op,
                distance: *distance,
                left: 1,
            });
            flat(&node.child[0], out);
            if let Some(l) = node.child.get(1) {
                let left = out.len() - at;
                if let Item::Opr { left: slot, .. } = &mut out[at] {
                    *slot = left;
                }
                flat(l, out);
            }
        }
    }
}

/// A tree with each NOT and what it holds taken out, an AND or a phrase left with its other
/// operand, an OR losing either losing both: `clean_NOT_intree` (tsquery_cleanup.c).
fn clean_not(node: Node) -> Option<Node> {
    let Some(op) = node.op() else {
        return Some(node);
    };
    let mut child = node.child;
    match op {
        Op::Not => None,
        Op::Or => {
            let l = clean_not(child.pop().expect("a left operand"))?;
            let r = clean_not(child.pop().expect("a right operand"))?;
            Some(Node::opr(Op::Or, 0, vec![r, l]))
        }
        Op::And | Op::Phrase => {
            let l = clean_not(child.pop().expect("a left operand"));
            let r = clean_not(child.pop().expect("a right operand"));
            match (l, r) {
                (None, None) => None,
                (None, Some(r)) => Some(r),
                (Some(l), None) => Some(l),
                (Some(l), Some(r)) => Some(Node::opr(op, distance_of(&node.kind), vec![r, l])),
            }
        }
    }
}

fn distance_of(kind: &Kind) -> u16 {
    match kind {
        Kind::Opr { distance, .. } => *distance,
        _ => 0,
    }
}

/// `querytree(tsquery)`: the tsquery without its NOTs, as `tsqueryout` writes one; `T` where
/// nothing is left, and the empty text for the empty tsquery (`tsquerytree`, tsquery.c).
pub fn querytree(query: &str) -> String {
    let items = held_query(query);
    if items.is_empty() {
        return String::new();
    }
    on_stack(depth(&items), || match clean_not(tree(&items, 0)) {
        None => "T".to_string(),
        Some(n) => {
            let mut out = Vec::new();
            flat(&n, &mut out);
            infix(&out, false)
        }
    })
}

/// Two nodes in the order `QTNodeCompare` (tsquery_util.c) gives them: operators before operands;
/// operators by their code, the greater first, their count of operands, the greater first, each
/// operand in turn, then a phrase's distance, the greater first; operands by their CRC, the
/// greater first, then their bytes.
fn compare(a: &Node, b: &Node) -> Ordering {
    let (ta, tb) = (a.kind.code(), b.kind.code());
    if ta != tb {
        return tb.cmp(&ta);
    }
    match (&a.kind, &b.kind) {
        (
            Kind::Opr {
                op: ao,
                distance: ad,
            },
            Kind::Opr {
                op: bo,
                distance: bd,
            },
        ) => {
            if ao != bo {
                return (*bo as u8).cmp(&(*ao as u8));
            }
            if a.child.len() != b.child.len() {
                return b.child.len().cmp(&a.child.len());
            }
            for (x, y) in a.child.iter().zip(&b.child) {
                let r = compare(x, y);
                if r != Ordering::Equal {
                    return r;
                }
            }
            if *ao == Op::Phrase && ad != bd {
                return bd.cmp(ad);
            }
            Ordering::Equal
        }
        (Kind::Val { word: aw, .. }, Kind::Val { word: bw, .. }) => {
            let (ca, cb) = (legacy_crc32(aw.as_bytes()), legacy_crc32(bw.as_bytes()));
            if ca != cb {
                return cb.cmp(&ca);
            }
            aw.as_bytes().cmp(bw.as_bytes())
        }
        _ => Ordering::Equal,
    }
}

/// The operands of each AND and each OR sorted as `compare` orders them, a phrase's kept in
/// place: `QTNSort`.
fn sort_tree(node: &mut Node) {
    if node.op().is_none() {
        return;
    }
    for c in &mut node.child {
        sort_tree(c);
    }
    if node.child.len() > 1 && node.op() != Some(Op::Phrase) {
        pg_qsort(&mut node.child, &compare);
    }
}

/// An AND's operands that are ANDs themselves, and an OR's that are ORs, replaced by their own
/// operands: `QTNTernary`.
fn ternary(node: &mut Node) {
    let Some(op) = node.op() else {
        return;
    };
    for c in &mut node.child {
        ternary(c);
    }
    if op != Op::And && op != Op::Or {
        return;
    }
    let old = std::mem::take(&mut node.child);
    for c in old {
        if c.op() == Some(op) {
            node.child.extend(c.child);
        } else {
            node.child.push(c);
        }
    }
}

/// An operator over more than two operands made a chain of operators over two each: `QTNBinary`.
fn binary(node: &mut Node) {
    let Some(op) = node.op() else {
        return;
    };
    for c in &mut node.child {
        binary(c);
    }
    let ch = &mut node.child;
    while ch.len() > 2 {
        let last = ch.pop().expect("an operand");
        let c0 = std::mem::replace(&mut ch[0], Node::stop());
        let c1 = std::mem::replace(&mut ch[1], last);
        ch[0] = Node::opr(op, 0, vec![c0, c1]);
    }
}

/// `node` with `ex` replaced by a copy of `subs`, or by nothing where none is given: the whole
/// node where it equals `ex`, or those of an AND's or OR's operands that are `ex`'s, the copy put
/// among the rest: `findeq` (tsquery_rewrite.c).
fn findeq(node: Node, ex: &Node, subs: Option<&Node>) -> Option<Node> {
    if node.sign & ex.sign != ex.sign || node.kind.code() != ex.kind.code() || node.nochange {
        return Some(node);
    }
    let replaced = || {
        subs.map(|s| {
            let mut s = s.clone();
            s.nochange = true;
            s
        })
    };
    match &node.kind {
        Kind::Opr { op, .. } => {
            if Some(*op) != ex.op() {
                return Some(node);
            }
            if node.child.len() == ex.child.len() {
                if node.sign == ex.sign && compare(&node, ex) == Ordering::Equal {
                    return replaced();
                }
            } else if node.child.len() > ex.child.len() && !ex.child.is_empty() {
                let mut matched = vec![false; node.child.len()];
                let (mut i, mut j, mut n) = (0, 0, 0);
                while i < node.child.len() && j < ex.child.len() {
                    match compare(&node.child[i], &ex.child[j]) {
                        Ordering::Equal => {
                            matched[i] = true;
                            n += 1;
                            i += 1;
                            j += 1;
                        }
                        Ordering::Less => i += 1,
                        Ordering::Greater => break,
                    }
                }
                if n == ex.child.len() {
                    let mut node = node;
                    let mut k = 0;
                    node.child.retain(|_| {
                        k += 1;
                        !matched[k - 1]
                    });
                    if let Some(s) = replaced() {
                        node.child.push(s);
                    }
                    sort_tree(&mut node);
                    return Some(node);
                }
            }
            Some(node)
        }
        Kind::Val { word, .. } => {
            let Kind::Val { word: exw, .. } = &ex.kind else {
                return Some(node);
            };
            if legacy_crc32(word.as_bytes()) == legacy_crc32(exw.as_bytes())
                && node.sign == ex.sign
                && compare(&node, ex) == Ordering::Equal
            {
                return replaced();
            }
            Some(node)
        }
        Kind::Stop => Some(node),
    }
}

/// `ex` replaced throughout the tree, an operator left with no operand dropped, and one other than
/// NOT left with one replaced by it: `dofindsubquery`.
fn find_sub(root: Node, ex: &Node, subs: Option<&Node>) -> Option<Node> {
    let mut root = findeq(root, ex, subs)?;
    if root.nochange || root.op().is_none() {
        return Some(root);
    }
    let old = std::mem::take(&mut root.child);
    for c in old {
        if let Some(c) = find_sub(c, ex, subs) {
            root.child.push(c);
        }
    }
    match root.child.len() {
        0 => None,
        1 if root.op() != Some(Op::Not) => root.child.pop(),
        _ => Some(root),
    }
}

/// `ts_rewrite(query, target, substitute)`: every occurrence of `target` in `query` replaced by
/// `substitute`, the empty tsquery for none, after the query's ANDs and ORs are flattened and
/// their operands sorted as `QTNodeCompare` orders them (`tsquery_rewrite`).
pub fn ts_rewrite(query: &str, target: &str, substitute: &str) -> Result<String> {
    let items = held_query(query);
    let ex = held_query(target);
    if items.is_empty() || ex.is_empty() {
        return Ok(query.to_string());
    }
    let subst = held_query(substitute);
    let deep = depth(&items).max(depth(&ex)).max(depth(&subst));
    on_stack(deep, || {
        let mut root = tree(&items, 0);
        ternary(&mut root);
        sort_tree(&mut root);
        let mut ex = tree(&ex, 0);
        ternary(&mut ex);
        sort_tree(&mut ex);
        let subs = (!subst.is_empty()).then(|| tree(&subst, 0));
        match find_sub(root, &ex, subs.as_ref()) {
            None => Ok(String::new()),
            Some(mut root) => {
                binary(&mut root);
                let mut out = Vec::new();
                flat(&root, &mut out);
                Ok(infix(&packed(out)?, true))
            }
        }
    })
}

/// The weights of D, C, B and A when none are given.
const DEFAULT_WEIGHTS: [f32; 4] = [0.1, 0.2, 0.4, 1.0];

/// The normalization's bits.
const NORM_LOGLENGTH: i32 = 0x01;
const NORM_LENGTH: i32 = 0x02;
const NORM_EXTDIST: i32 = 0x04;
const NORM_UNIQ: i32 = 0x08;
const NORM_LOGUNIQ: i32 = 0x10;
const NORM_RDIVRPLUS1: i32 = 0x20;

/// The weights of D, C, B and A from a real[] as `getWeights` (tsrank.c) reads them: the first
/// four, a negative one or NaN taken as its default; the array's other elements read only for
/// NULLs. An empty slice is the empty array, which has no dimension.
fn weights_of(given: Option<&[Option<f32>]>) -> Result<[f32; 4]> {
    let Some(a) = given else {
        return Ok(DEFAULT_WEIGHTS);
    };
    if a.is_empty() {
        return Err(Error::new("array of weight must be one-dimensional"));
    }
    if a.len() < 4 {
        return Err(Error::new("array of weight is too short"));
    }
    if a.iter().any(Option::is_none) {
        return Err(Error::new("array of weight must not contain nulls"));
    }
    let mut ws = [0f32; 4];
    for i in 0..4 {
        let w = a[i].expect("a weight");
        ws[i] = if w >= 0.0 { w } else { DEFAULT_WEIGHTS[i] };
        if ws[i] > 1.0 {
            return Err(Error::new("weight out of range"));
        }
    }
    Ok(ws)
}

/// The weight of a word `w` positions from another: `word_distance` (tsrank.c).
fn word_distance(w: i32) -> f32 {
    if w > 100 {
        return 1e-30;
    }
    (1.0 / (1.005 + 0.05 * ((w as f32) as f64 / 1.5 - 2.0).exp())) as f32
}

/// A tsvector's count of positions, a lexeme without any counted once: `cnt_length`.
fn cnt_length(v: &[Entry]) -> usize {
    v.iter().map(|e| e.pos.len().max(1)).sum()
}

/// The lexemes of a tsvector an operand finds: the one equal to it, or for a prefix those it
/// starts: `find_wordentry`.
fn find_entries(v: &[Entry], word: &str, prefix: bool) -> std::ops::Range<usize> {
    let w = word.as_bytes();
    let (mut lo, mut hi) = (0, v.len());
    let mut n = 0;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        match w.cmp(v[mid].word.as_bytes()) {
            Ordering::Equal => {
                hi = mid;
                n = 1;
                break;
            }
            Ordering::Greater => lo = mid + 1,
            Ordering::Less => hi = mid,
        }
    }
    if prefix {
        n = 0;
        while hi + n < v.len() && v[hi + n].word.as_bytes().starts_with(w) {
            n += 1;
        }
    }
    hi..hi + n
}

/// The indexes of a tsquery's operands, sorted by their bytes, one kept of each: the first
/// `qsort_arg` leaves: `SortAndUniqItems`.
fn sort_and_uniq(items: &[Item]) -> Vec<usize> {
    let word = |k: usize| match &items[k] {
        Item::Val { word, .. } => word.as_bytes(),
        Item::Opr { .. } => b"",
    };
    let mut res: Vec<usize> = (0..items.len())
        .filter(|&k| matches!(items[k], Item::Val { .. }))
        .collect();
    if res.len() < 2 {
        return res;
    }
    pg_qsort(&mut res, &|a, b| word(*a).cmp(word(*b)));
    res.dedup_by(|b, a| word(*a) == word(*b));
    res
}

fn operand(items: &[Item], k: usize) -> (&str, u8, bool) {
    match &items[k] {
        Item::Val {
            word,
            weight,
            prefix,
        } => (word, *weight, *prefix),
        Item::Opr { .. } => ("", 0, false),
    }
}

/// The rank of a tsquery whose top is an AND or a phrase: each pair of its distinct operands'
/// positions, near ones weighing more: `calc_rank_and`.
fn calc_rank_and(w: &[f32; 4], v: &[Entry], items: &[Item]) -> f32 {
    let ops = sort_and_uniq(items);
    if ops.len() < 2 {
        return calc_rank_or(w, v, items);
    }
    const POSNULL: [u16; 1] = [POSITION_MAX];
    let mut pos: Vec<Option<(&[u16], bool)>> = vec![None; ops.len()];
    let mut res: f32 = -1.0;
    let wpos = |p: u16| w[weight_of(p) as usize];
    for i in 0..ops.len() {
        let (word, _, prefix) = operand(items, ops[i]);
        for e in find_entries(v, word, prefix) {
            pos[i] = Some(if v[e].pos.is_empty() {
                (&POSNULL[..], true)
            } else {
                (&v[e].pos[..], false)
            });
            let (post, inull) = pos[i].expect("positions");
            for slot in &pos[..i] {
                let Some((ct, knull)) = *slot else {
                    continue;
                };
                for &l in post {
                    for &p in ct {
                        let mut dist = (pos_of(l) as i32 - pos_of(p) as i32).abs();
                        if dist != 0 || inull || knull {
                            if dist == 0 {
                                dist = POSITION_MAX as i32 + 1;
                            }
                            let curw =
                                ((wpos(l) * wpos(p) * word_distance(dist)) as f64).sqrt() as f32;
                            res = if res < 0.0 {
                                curw
                            } else {
                                (1.0 - (1.0 - res as f64) * (1.0 - curw as f64)) as f32
                            };
                        }
                    }
                }
            }
        }
    }
    res
}

/// The rank of a tsquery whose top is not an AND or a phrase: each distinct operand's positions'
/// weights, the greatest first, over the squares of their places: `calc_rank_or`.
fn calc_rank_or(w: &[f32; 4], v: &[Entry], items: &[Item]) -> f32 {
    let ops = sort_and_uniq(items);
    let mut res: f32 = 0.0;
    for &k in &ops {
        let (word, _, prefix) = operand(items, k);
        for e in find_entries(v, word, prefix) {
            let post: &[u16] = if v[e].pos.is_empty() { &[0] } else { &v[e].pos };
            let mut resj: f32 = 0.0;
            let mut wjm: f32 = -1.0;
            let mut jm: i32 = 0;
            for (j, &p) in post.iter().enumerate() {
                let j = j as i32;
                let wp = w[weight_of(p) as usize];
                resj += wp / ((j + 1) * (j + 1)) as f32;
                if wp > wjm {
                    wjm = wp;
                    jm = j;
                }
            }
            let term = resj - wjm / ((jm + 1) * (jm + 1)) as f32 + wjm;
            res = (res as f64 + term as f64 / 1.64493406685) as f32;
        }
    }
    if !ops.is_empty() {
        res /= ops.len() as f32;
    }
    res
}

/// `ts_rank([weights,] tsvector, tsquery [, normalization])`: `calc_rank` (tsrank.c), as a real.
/// `weights` are D's, C's, B's and A's, the defaults where none are given; the normalization 0
/// where none is.
pub fn ts_rank(
    weights: Option<&[Option<f32>]>,
    vector: &str,
    query: &str,
    normalization: i32,
) -> Result<f32> {
    let w = weights_of(weights)?;
    let v = held_vector(vector);
    let items = held_query(query);
    if v.is_empty() || items.is_empty() {
        return Ok(0.0);
    }
    let and = matches!(
        items[0],
        Item::Opr {
            op: Op::And | Op::Phrase,
            ..
        }
    );
    let mut res = if and {
        calc_rank_and(&w, &v, &items)
    } else {
        calc_rank_or(&w, &v, &items)
    };
    if res < 0.0 {
        res = 1e-20;
    }
    let method = normalization;
    if method & NORM_LOGLENGTH != 0 {
        res = (res as f64 / (((cnt_length(&v) + 1) as f64).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(&v);
        if len > 0 {
            res /= len as f32;
        }
    }
    if method & NORM_UNIQ != 0 {
        res /= v.len() as f32;
    }
    if method & NORM_LOGUNIQ != 0 {
        res = (res as f64 / (((v.len() + 1) as f64).ln() / 2f64.ln())) as f32;
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        res /= res + 1.0;
    }
    Ok(res)
}

/// A position of the document a tsquery's operands find, as `DocRepresentation` holds one: the
/// position with its weight, the lexeme's index, and the operands found there.
struct DocPos {
    pos: u16,
    entry: usize,
    items: Vec<usize>,
}

/// The positions of a tsvector the operands of a tsquery find, those of the weights each asks
/// for, sorted by position, weight and lexeme, the operands at one position of one lexeme joined:
/// `get_docrep`.
fn docrep(v: &[Entry], items: &[Item]) -> Vec<DocPos> {
    let mut doc: Vec<DocPos> = Vec::new();
    for (k, item) in items.iter().enumerate() {
        let Item::Val {
            word,
            weight,
            prefix,
        } = item
        else {
            continue;
        };
        for e in find_entries(v, word, *prefix) {
            for &p in &v[e].pos {
                if *weight == 0 || *weight & (1 << weight_of(p)) != 0 {
                    doc.push(DocPos {
                        pos: p,
                        entry: e,
                        items: vec![k],
                    });
                }
            }
        }
    }
    pg_qsort(&mut doc, &|a, b| {
        pos_of(a.pos)
            .cmp(&pos_of(b.pos))
            .then(weight_of(a.pos).cmp(&weight_of(b.pos)))
            .then(a.entry.cmp(&b.entry))
    });
    let mut out: Vec<DocPos> = Vec::with_capacity(doc.len());
    for d in doc {
        match out.last_mut() {
            Some(last) if last.pos == d.pos && last.entry == d.entry => last.items.extend(d.items),
            _ => out.push(d),
        }
    }
    out
}

/// Where each operand of a tsquery was found while a cover is sought: `QueryRepresentationOperand`,
/// its positions in the order they were met.
#[derive(Clone, Default)]
struct Seen {
    exists: bool,
    pos: Vec<u16>,
}

/// A cover: the next search's start, the cover's first and last positions and its first and last
/// places in the document: `CoverExt`.
#[derive(Default)]
struct Cover {
    pos: usize,
    p: i32,
    q: i32,
    begin: usize,
    end: usize,
}

/// Whether the operands seen so far match the tsquery, each seen operand's positions ascending:
/// `TS_execute` with `checkcondition_QueryOperand`.
fn seen_matches(items: &[Item], seen: &[Seen], reverse: bool) -> bool {
    let cond = |k: usize, data: Option<&mut Found>| {
        let s = &seen[k];
        if !s.exists {
            return Ternary::No;
        }
        if let Some(d) = data {
            d.pos = s.pos.clone();
            if reverse {
                d.pos.reverse();
            }
        }
        Ternary::Yes
    };
    execute(items, 0, &cond) != Ternary::No
}

/// The operands found at a place of the document marked seen, a position kept where it is not
/// the last one kept: `fillQueryRepresentationData`.
fn fill(seen: &mut [Seen], d: &DocPos) {
    for &k in &d.items {
        let s = &mut seen[k];
        s.exists = true;
        if s.pos.last().is_none_or(|&l| pos_of(l) != pos_of(d.pos)) {
            s.pos.push(d.pos);
        }
    }
}

/// The next cover from `ext.pos`: the first place where the tsquery is matched reading forward,
/// then the nearest place back from it where it is matched reading backward: `Cover`.
fn next_cover(doc: &[DocPos], items: &[Item], ext: &mut Cover) -> bool {
    loop {
        let mut seen = vec![Seen::default(); items.len()];
        ext.p = i32::MAX;
        ext.q = 0;
        let mut lastpos = ext.pos;
        let mut found = false;
        let mut k = ext.pos;
        while k < doc.len() {
            fill(&mut seen, &doc[k]);
            if seen_matches(items, &seen, false) {
                if pos_of(doc[k].pos) as i32 > ext.q {
                    ext.q = pos_of(doc[k].pos) as i32;
                    ext.end = k;
                    lastpos = k;
                    found = true;
                }
                break;
            }
            k += 1;
        }
        if !found {
            return false;
        }
        let mut seen = vec![Seen::default(); items.len()];
        let mut k = lastpos as isize;
        while k >= ext.pos as isize {
            fill(&mut seen, &doc[k as usize]);
            if seen_matches(items, &seen, true) {
                if (pos_of(doc[k as usize].pos) as i32) < ext.p {
                    ext.begin = k as usize;
                    ext.p = pos_of(doc[k as usize].pos) as i32;
                }
                break;
            }
            k -= 1;
        }
        if ext.p <= ext.q {
            ext.pos = (k + 1) as usize;
            return true;
        }
        ext.pos += 1;
    }
}

/// `ts_rank_cd([weights,] tsvector, tsquery [, normalization])`: `calc_rank_cd` (tsrank.c), as a
/// real: the covers of the tsquery in the document, each weighing its places' weights over its
/// length.
pub fn ts_rank_cd(
    weights: Option<&[Option<f32>]>,
    vector: &str,
    query: &str,
    normalization: i32,
) -> Result<f32> {
    let w = weights_of(weights)?;
    let mut invws = [0f64; 4];
    for i in 0..4 {
        invws[i] = w[i] as f64;
        if invws[i] > 1.0 {
            return Err(Error::new("weight out of range"));
        }
        invws[i] = 1.0 / invws[i];
    }
    let v = held_vector(vector);
    let items = held_query(query);
    let doc = docrep(&v, &items);
    if doc.is_empty() {
        return Ok(0.0);
    }
    let method = normalization;
    let (mut wdoc, mut sum_dist, mut prev) = (0f64, 0f64, 0f64);
    let mut n_extent = 0;
    let mut ext = Cover::default();
    on_stack(depth(&items), || {
        while next_cover(&doc, &items, &mut ext) {
            let mut inv_sum = 0f64;
            for d in &doc[ext.begin..=ext.end] {
                inv_sum += invws[weight_of(d.pos) as usize];
            }
            let span = (ext.end - ext.begin) as i32;
            let cpos = (span + 1) as f64 / inv_sum;
            let mut noise = (ext.q - ext.p) - span;
            if noise < 0 {
                noise = span / 2;
            }
            wdoc += cpos / (1 + noise) as f64;
            let cur = (ext.q + ext.p) as f64 / 2.0;
            if n_extent > 0 && cur > prev {
                sum_dist += 1.0 / (cur - prev);
            }
            prev = cur;
            n_extent += 1;
        }
    });
    if method & NORM_LOGLENGTH != 0 {
        wdoc /= ((cnt_length(&v) + 1) as f64).ln();
    }
    if method & NORM_LENGTH != 0 {
        let len = cnt_length(&v);
        if len > 0 {
            wdoc /= len as f64;
        }
    }
    if method & NORM_EXTDIST != 0 && n_extent > 0 && sum_dist > 0.0 {
        wdoc /= n_extent as f64 / sum_dist;
    }
    if method & NORM_UNIQ != 0 {
        wdoc /= v.len() as f64;
    }
    if method & NORM_LOGUNIQ != 0 {
        wdoc /= ((v.len() + 1) as f64).ln() / 2f64.ln();
    }
    if method & NORM_RDIVRPLUS1 != 0 {
        wdoc /= wdoc + 1.0;
    }
    Ok(wdoc as f32)
}

#[cfg(test)]
mod tests {
    use super::super::{tsquery_in, tsquery_out, tsvector_in};
    use super::*;

    // Every expected value below is PostgreSQL 19's, read on its server.

    fn v(s: &str) -> String {
        tsvector_in(s).unwrap()
    }

    fn q(s: &str) -> String {
        tsquery_in(s).unwrap()
    }

    fn arr(a: &[&str]) -> Vec<Option<String>> {
        a.iter().map(|s| Some(s.to_string())).collect()
    }

    #[test]
    fn a_tsvector_is_edited_as_tsvector_op_edits_it() {
        let base = v("base:7 hidden:6 rebel:1 spaceship:2,33A,34B,35C,36D strike:3");
        assert_eq!(ts_length(&base), 5);
        assert_eq!(
            strip(&v("w:12B w:13* w:12,5,6 a:1,3* a:3 w asd:1dc asd")),
            "'a' 'asd' 'w'"
        );
        assert_eq!(
            ts_delete_lexemes(&base, &arr(&["spaceshi", "rebel"])),
            "'base':7 'hidden':6 'spaceship':2,33A,34B,35C,36 'strike':3"
        );
        assert_eq!(
            ts_delete(&base, "base"),
            "'hidden':6 'rebel':1 'spaceship':2,33A,34B,35C,36 'strike':3"
        );
        let w = v("a:1,3A asd:1C w:5,6,12B,13A zxc:81,222A,567");
        assert_eq!(
            setweight_lexemes(&w, b'c', &arr(&["a", "zxc"])).unwrap(),
            "'a':1C,3C 'asd':1C 'w':5,6,12B,13A 'zxc':81C,222C,567C"
        );
        assert_eq!(
            setweight(&w, b'A').unwrap(),
            "'a':1A,3A 'asd':1A 'w':5A,6A,12A,13A 'zxc':81A,222A,567A"
        );
        assert_eq!(
            setweight(&w, b'x').unwrap_err().0,
            "unrecognized weight: \"x\""
        );
        assert_eq!(
            setweight(&w, 0xe9).unwrap_err().0,
            "unrecognized weight: \"\\351\""
        );
        assert_eq!(
            ts_filter(
                &v("base:7A empir:17 hidden:6A rebel:1A spaceship:2A,3B victori:12"),
                &[Some(b'a')]
            )
            .unwrap(),
            "'base':7A 'hidden':6A 'rebel':1A 'spaceship':2A"
        );
        assert_eq!(
            ts_filter(&v("base hidden"), &[Some(b'a'), None])
                .unwrap_err()
                .0,
            "weight array may not contain nulls"
        );
        assert_eq!(tsvector_to_array(&v("b a:3")), ["a", "b"]);
        assert_eq!(
            array_to_tsvector(&arr(&["foo", "bar", "baz", "bar"])).unwrap(),
            "'bar' 'baz' 'foo'"
        );
        assert_eq!(
            array_to_tsvector(&[Some("a".into()), Some(String::new())])
                .unwrap_err()
                .0,
            "lexeme array may not contain empty strings"
        );
        let rows = unnest(&v("a:1,2A b"));
        assert_eq!(rows[0].positions, Some(vec![1, 2]));
        assert_eq!(
            rows[0].weights,
            Some(vec!["D".to_string(), "A".to_string()])
        );
        assert_eq!(rows[1].positions, None);
    }

    #[test]
    fn a_tsquery_is_cleaned_and_rewritten_as_postgres_does() {
        assert_eq!(numnode(&q("new & york | qwery")), 5);
        assert_eq!(querytree(&q("new & !york")), "'new'");
        assert_eq!(querytree(&q("!york")), "T");
        assert_eq!(querytree(&q("!a | b")), "T");
        assert_eq!(querytree(&q("a <-> !b & c")), "'a' & 'c'");
        assert_eq!(querytree(""), "");
        let r = |a: &str, b: &str, c: &str| tsquery_out(&ts_rewrite(&q(a), &q(b), &q(c)).unwrap());
        assert_eq!(
            r(
                "foo & bar & qq & new & york",
                "new & york",
                "big & apple | nyc | new & york & city"
            ),
            "'foo' & 'bar' & 'qq' & ( 'city' & 'new' & 'york' | 'nyc' | 'big' & 'apple' )"
        );
        assert_eq!(
            r("'foo' <-> 'foo'", "foo", "bar | baz"),
            "( 'bar' | 'baz' ) <-> ( 'bar' | 'baz' )"
        );
        assert_eq!(r("5 & (6 | 5)", "5", ""), "'6'");
        assert_eq!(r("!5", "5", ""), "");
        assert_eq!(r("b & a", "x", "y"), "'b' & 'a'");
    }

    #[test]
    fn ranks_are_tsranks_digit_for_digit() {
        let rank = |w: Option<&[Option<f32>]>, a: &str, b: &str, n: i32, cd: bool| {
            let (a, b) = (v(a), q(b));
            let r = if cd {
                ts_rank_cd(w, &a, &b, n)
            } else {
                ts_rank(w, &a, &b, n)
            };
            r.unwrap()
        };
        assert_eq!(rank(None, " a:1 s:2C d g", "a | s", 0, false), 0.091189064);
        assert_eq!(rank(None, "a:1,2A,3B", "a | b", 0, false), 0.3478694);
        assert_eq!(rank(None, " a:1 s:2C d g", "a & s", 0, false), 0.14015312);
        assert_eq!(rank(None, " a:1 sa:2C d g", "a | s:*", 0, true), 0.3);
        assert_eq!(
            rank(None, " a:1 sa:3C sab:2c d g", "a | sa:*", 0, true),
            0.5
        );
        assert_eq!(
            rank(None, " a:1 s:2,3A d:2A g", "a <2> s:A", 0, true),
            0.09090909
        );
        assert_eq!(
            rank(None, " a:1 sa:2A sb:2D g", "a <-> s:* <-> sa:A", 0, true),
            0.0
        );
        let w = [Some(0.1), Some(0.2), Some(0.4), Some(1.0)];
        assert_eq!(rank(Some(&w), "a:1 b:3", "a & b", 0, false), 0.098500855);
        assert_eq!(
            ts_rank(Some(&w[..3]), "'a'", "'a'", 0).unwrap_err().0,
            "array of weight is too short"
        );
        assert_eq!(
            ts_rank_cd(Some(&[Some(2.0); 4]), "'a'", "'a'", 0)
                .unwrap_err()
                .0,
            "weight out of range"
        );
    }
}
