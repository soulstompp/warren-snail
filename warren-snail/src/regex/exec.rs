//! The executor (regexec.c): a string searched for a compiled pattern as PostgreSQL 19's
//! `pg_regexec` searches it. The search NFA's DFA finds where a match can start and end; the whole
//! pattern's DFA then pins the match down; and where the pattern captures or refers back, its tree
//! of subexpressions is dissected over the match, backtracking where a back reference fails.

use super::dfa::{longest, newdfa, shortest, Dfa};
use super::guts::{
    Chr, Guts, RegError, RegMatch, DUPINF, REG_ASSERT, REG_ESPACE, REG_ETOOBIG, REG_EXPECT,
    REG_INVARG, REG_NOMATCH, REG_NOSUB, REG_OKAY, REG_UBACKREF, REG_UIMPOSSIBLE, SHORTER,
};

/// How deep the dissection of the tree of subexpressions and the testing of lookaround
/// constraints may nest before the execution is refused as too complex, where PostgreSQL measures
/// its stack (`STACK_TOO_DEEP`): a level of the dissection counts `DISSECT_DEPTH`, the test of a
/// constraint `LACON_DEPTH`, each by the stack it takes.
pub(super) const MAX_DEPTH: usize = 1000;
pub(super) const DISSECT_DEPTH: usize = 1;
pub(super) const LACON_DEPTH: usize = 4;

/// The greatest allocation PostgreSQL makes (`MaxAllocSize`).
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;

/// What one execution carries (`struct vars`).
pub(super) struct Vars<'a> {
    pub(super) g: &'a Guts,
    pub(super) eflags: i32,
    /// The matches worked on, `nmatch` of them.
    pub(super) nmatch: usize,
    pub(super) pmatch: Vec<RegMatch>,
    /// The string; its start is place 0.
    pub(super) string: &'a [Chr],
    pub(super) search_start: usize,
    /// Just past the string's end.
    pub(super) stop: usize,
    /// The first error seen, 0 for none.
    pub(super) err: i32,
    /// Each tree node's DFA, by id, made the first time it is asked for.
    subdfas: Vec<Option<Box<Dfa<'a>>>>,
    /// Each lookaround constraint's DFA, by number, and where its lookbehind scan stands.
    pub(super) ladfas: Vec<Option<Box<Dfa<'a>>>>,
    pub(super) lblastcss: Vec<Option<usize>>,
    pub(super) lblastcp: Vec<Option<usize>>,
    /// How deep the dissection and the constraints' tests nest now.
    pub(super) depth: usize,
    /// `REG_EXPECT`'s `rm_extend`, once written.
    extend: Option<RegMatch>,
}

impl Vars<'_> {
    /// Records an error, unless one is recorded already (`ERR`).
    pub(super) fn seterr(&mut self, e: i32) {
        if self.err == 0 {
            self.err = e;
        }
    }

    /// Whether one more level of nesting, counting `cost`, would pass the bound
    /// (`STACK_TOO_DEEP`).
    pub(super) fn too_deep(&self, cost: usize) -> bool {
        self.depth + cost > MAX_DEPTH
    }

    /// Records `REG_EXPECT`'s details: from the last place a match could still have started (the
    /// string's end where none is known) to the string's end.
    fn expect(&mut self, cold: Option<usize>) {
        if self.g.cflags & REG_EXPECT != 0 {
            self.extend = Some(RegMatch {
                rm_so: cold.unwrap_or(self.stop) as i64,
                rm_eo: self.stop as i64,
            });
        }
    }
}

/// `pg_regexec`: the string searched from `search_start`; the first `nmatch` matches given back
/// where it matches (the whole match first, then each capture's, -1 and -1 for one that matched
/// nothing), None where it does not; `details`, where given, gets the `rm_extend` of `REG_EXPECT`.
pub(super) fn exec(
    g: &Guts,
    string: &[Chr],
    search_start: usize,
    nmatch: usize,
    eflags: i32,
    details: Option<&mut RegMatch>,
) -> Result<Option<Vec<RegMatch>>, RegError> {
    if search_start > string.len() {
        return Ok(None);
    }
    if g.cflags & REG_EXPECT != 0 && details.is_none() {
        return Err(RegError(REG_INVARG));
    }
    if g.info & REG_UIMPOSSIBLE != 0 {
        return Ok(None);
    }
    let backref = g.info & REG_UBACKREF != 0;

    // A pattern that refers back needs every capture's match, wanted or not.
    let work = if backref && nmatch <= g.nsub {
        g.nsub + 1
    } else {
        nmatch.min(g.nsub + 1)
    };

    let ntree = g.ntree.max(g.subres.len());
    let nlacons = g.lacons.len();
    let mut v = Vars {
        g,
        eflags,
        nmatch: work,
        pmatch: vec![RegMatch::UNSET; work],
        string,
        search_start,
        stop: string.len(),
        err: 0,
        subdfas: (0..ntree).map(|_| None).collect(),
        ladfas: (0..nlacons).map(|_| None).collect(),
        lblastcss: vec![None; nlacons],
        lblastcp: vec![None; nlacons],
        depth: 0,
        extend: None,
    };

    let st = if backref { cfind(&mut v) } else { find(&mut v) };

    if let (Some(details), Some(extend)) = (details, v.extend) {
        *details = extend;
    }

    match st {
        REG_OKAY => {
            let mut pmatch = vec![RegMatch::UNSET; nmatch];
            let n = nmatch.min(work);
            pmatch[..n].copy_from_slice(&v.pmatch[..n]);
            // Captures' matches, possibly partial, are not shown where none were asked for.
            if g.cflags & REG_NOSUB != 0 {
                zapallsubs(&mut pmatch);
            }
            Ok(Some(pmatch))
        }
        REG_NOMATCH => Ok(None),
        e => Err(RegError(e)),
    }
}

/// `getsubdfa`: a tree node's DFA, made the first time it is asked for, taken out of the
/// execution's keeping until `putsubdfa` gives it back; None on an error (`v.err` set). Only a
/// node's parent asks for its DFA, so no two callers hold one at once.
fn getsubdfa<'a>(v: &mut Vars<'a>, t: usize) -> Option<Box<Dfa<'a>>> {
    if let Some(d) = v.subdfas[t].take() {
        return Some(d);
    }
    let g = v.g;
    let sub = &g.subres[t];
    let mut d = Box::new(newdfa(v, &sub.cnfa, &g.cmap)?);
    if sub.op == b'b' {
        d.backno = sub.backno as i32;
        d.backmin = sub.min;
        d.backmax = sub.max;
    }
    Some(d)
}

/// The DFA `getsubdfa` gave, back in the execution's keeping.
fn putsubdfa<'a>(v: &mut Vars<'a>, t: usize, d: Box<Dfa<'a>>) {
    v.subdfas[t] = Some(d);
}

/// `find`: the match of a pattern that does not refer back. The search NFA finds the range of
/// places a match can start in and where the first one ends; the pattern's own DFA then tries each
/// start in turn; the tree is dissected only for the captures' matches.
fn find(v: &mut Vars) -> i32 {
    let g = v.g;
    let tree = g.tree();
    let shorter = tree.flags & SHORTER != 0;

    // A shot with the search NFA.
    let Some(mut s) = newdfa(v, &g.search, &g.cmap) else {
        return v.err;
    };
    let mut cold = None;
    let (from, stop) = (v.search_start, v.stop);
    let close = shortest(v, &mut s, from, from, stop, Some(&mut cold), None);
    drop(s);
    if v.err != 0 {
        return v.err;
    }
    v.expect(cold);
    let Some(close) = close else {
        return REG_NOMATCH;
    };
    if v.nmatch == 0 {
        return REG_OKAY;
    }

    // The start, and the match.
    let Some(open) = cold else {
        return REG_ASSERT;
    };
    let mut cold = None;
    let Some(mut d) = newdfa(v, &tree.cnfa, &g.cmap) else {
        return v.err;
    };
    let mut begin = open;
    let mut end = None;
    while begin <= close {
        let mut hitend = false;
        end = if shorter {
            shortest(v, &mut d, begin, begin, stop, None, Some(&mut hitend))
        } else {
            longest(v, &mut d, begin, stop, Some(&mut hitend))
        };
        if v.err != 0 {
            return v.err;
        }
        if hitend && cold.is_none() {
            cold = Some(begin);
        }
        if end.is_some() {
            break;
        }
        begin += 1;
    }
    drop(d);
    // The search NFA found a match, so the loop does.
    let Some(end) = end else {
        return REG_ASSERT;
    };

    v.pmatch[0] = RegMatch {
        rm_so: begin as i64,
        rm_eo: end as i64,
    };
    v.expect(cold);
    if v.nmatch == 1 {
        return REG_OKAY;
    }

    // The captures' matches.
    cdissect(v, 1, begin, end)
}

/// `cfind`: the match of a pattern that refers back, by `cfindloop`.
fn cfind(v: &mut Vars) -> i32 {
    let g = v.g;
    let Some(mut s) = newdfa(v, &g.search, &g.cmap) else {
        return v.err;
    };
    let Some(mut d) = newdfa(v, &g.tree().cnfa, &g.cmap) else {
        return v.err;
    };

    let mut cold = None;
    let ret = cfindloop(v, &mut d, &mut s, &mut cold);

    drop(d);
    drop(s);
    if v.err != 0 {
        return v.err;
    }
    v.expect(cold);
    ret
}

/// `cfindloop`: the heart of `cfind`. The search NFA finds a range of places a match can start in;
/// at each, every end the pattern's DFA allows is tried, longest first (shortest first where the
/// pattern prefers the shortest), until the dissection of the tree accepts one; then the search
/// goes on past the range.
fn cfindloop(v: &mut Vars, d: &mut Dfa, s: &mut Dfa, coldp: &mut Option<usize>) -> i32 {
    let shorter = v.g.tree().flags & SHORTER != 0;
    let stop = v.stop;
    let mut cold = None;
    let mut close = v.search_start;
    loop {
        // The search NFA's range of starts at or past `close`.
        let found = shortest(v, s, close, close, stop, Some(&mut cold), None);
        if v.err != 0 {
            *coldp = cold;
            return v.err;
        }
        let Some(c) = found else {
            break;
        };
        close = c;
        let Some(open) = cold else {
            *coldp = cold;
            return REG_ASSERT;
        };
        cold = None;

        // Each match starting between `open` and `close`.
        for begin in open..=close {
            let mut estart = begin;
            let mut estop = stop;
            loop {
                let mut hitend = false;
                let end = if shorter {
                    shortest(v, d, begin, estart, estop, None, Some(&mut hitend))
                } else {
                    longest(v, d, begin, estop, Some(&mut hitend))
                };
                if v.err != 0 {
                    *coldp = cold;
                    return v.err;
                }
                if hitend && cold.is_none() {
                    cold = Some(begin);
                }
                let Some(end) = end else {
                    break;
                };

                // Whether the match really matches.
                let er = cdissect(v, 1, begin, end);
                if er == REG_OKAY {
                    if v.nmatch > 0 {
                        v.pmatch[0] = RegMatch {
                            rm_so: begin as i64,
                            rm_eo: end as i64,
                        };
                    }
                    *coldp = cold;
                    return REG_OKAY;
                }
                if er != REG_NOMATCH {
                    v.seterr(er);
                    *coldp = cold;
                    return er;
                }

                // The next longer or shorter match from the same start.
                if shorter {
                    if end == estop {
                        break;
                    }
                    estart = end + 1;
                } else {
                    if end == begin {
                        break;
                    }
                    estop = end - 1;
                }
            }
        }

        // No match starts at or before `close`: the search goes on past it.
        close += 1;
        if close >= stop {
            break;
        }
    }

    *coldp = cold;
    REG_NOMATCH
}

/// `zapallsubs`: every capture's match but the whole match's set to nothing.
fn zapallsubs(p: &mut [RegMatch]) {
    for m in p.iter_mut().skip(1) {
        *m = RegMatch::UNSET;
    }
}

/// `zaptreesubs`: the matches of the captures in a subtree set to nothing.
fn zaptreesubs(v: &mut Vars, t: usize) {
    let g = v.g;
    let mut pending = vec![t];
    while let Some(t) = pending.pop() {
        let sub = &g.subres[t];
        let n = sub.capno;
        if n > 0 && n < v.nmatch {
            v.pmatch[n] = RegMatch::UNSET;
        }
        let mut child = sub.child;
        while let Some(c) = child {
            pending.push(c);
            child = g.subres[c].sibling;
        }
    }
}

/// `subset`: a capture's match recorded, where it is among the matches worked on.
fn subset(v: &mut Vars, t: usize, begin: usize, end: usize) {
    let n = v.g.subres[t].capno;
    if n >= v.nmatch {
        return;
    }
    v.pmatch[n] = RegMatch {
        rm_so: begin as i64,
        rm_eo: end as i64,
    };
}

/// `cdissect`: the match from `begin` to `end` of tree node `t`, whose DFA the caller has run and
/// found to match it, dissected: back references checked, captures' matches recorded.
/// `REG_NOMATCH` where a back reference below does not hold, the captures' matches below cleared
/// again; an error code on an error.
fn cdissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    if v.too_deep(DISSECT_DEPTH) {
        return REG_ETOOBIG;
    }
    v.depth += DISSECT_DEPTH;

    let sub = &v.g.subres[t];
    let er = match sub.op {
        // A plain expression: its parent did the work.
        b'=' => REG_OKAY,
        b'b' => cbrdissect(v, t, begin, end),
        b'.' => match sub.child {
            Some(c) if v.g.subres[c].flags & SHORTER != 0 => crevcondissect(v, t, begin, end),
            Some(_) => ccondissect(v, t, begin, end),
            None => REG_ASSERT,
        },
        b'|' => caltdissect(v, t, begin, end),
        b'*' => match sub.child {
            Some(c) if v.g.subres[c].flags & SHORTER != 0 => creviterdissect(v, t, begin, end),
            Some(_) => citerdissect(v, t, begin, end),
            None => REG_ASSERT,
        },
        b'(' => match sub.child {
            Some(c) => cdissect(v, c, begin, end),
            None => REG_ASSERT,
        },
        _ => REG_ASSERT,
    };

    v.depth -= DISSECT_DEPTH;

    // A capturing node records its match.
    if sub.capno > 0 && er == REG_OKAY {
        subset(v, t, begin, end);
    }
    er
}

/// A concatenation's two children, by id.
fn children(v: &Vars, t: usize) -> Option<(usize, usize)> {
    let left = v.g.subres[t].child?;
    let right = v.g.subres[left].sibling?;
    Some((left, right))
}

/// `ccondissect`: a concatenation's match dissected, the split between its two children tried
/// from the left child's longest match down.
fn ccondissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let Some((left, right)) = children(v, t) else {
        return REG_ASSERT;
    };
    let Some(mut d) = getsubdfa(v, left) else {
        return v.err;
    };
    let Some(mut d2) = getsubdfa(v, right) else {
        putsubdfa(v, left, d);
        return v.err;
    };
    let er = ccondissect_with(v, left, right, &mut d, &mut d2, begin, end);
    putsubdfa(v, left, d);
    putsubdfa(v, right, d2);
    er
}

#[allow(clippy::too_many_arguments)]
fn ccondissect_with(
    v: &mut Vars,
    left: usize,
    right: usize,
    d: &mut Dfa,
    d2: &mut Dfa,
    begin: usize,
    end: usize,
) -> i32 {
    // A tentative split.
    let mid = longest(v, d, begin, end, None);
    if v.err != 0 {
        return v.err;
    }
    let Some(mut mid) = mid else {
        return REG_NOMATCH;
    };

    loop {
        // Whether the split works.
        if longest(v, d2, mid, end, None) == Some(end) {
            let mut er = cdissect(v, left, begin, mid);
            if er == REG_OKAY {
                er = cdissect(v, right, mid, end);
                if er == REG_OKAY {
                    return REG_OKAY;
                }
                // The left's matches cleared (the right cleared its own).
                zaptreesubs(v, left);
            }
            if er != REG_NOMATCH {
                return er;
            }
        }
        if v.err != 0 {
            return v.err;
        }

        // A new split, shorter on the left.
        if mid == begin {
            return REG_NOMATCH;
        }
        let next = longest(v, d, begin, mid - 1, None);
        if v.err != 0 {
            return v.err;
        }
        match next {
            Some(m) => mid = m,
            None => return REG_NOMATCH,
        }
    }
}

/// `crevcondissect`: a concatenation's match dissected where its left child prefers the shortest
/// match, the split tried from the left child's shortest match up.
fn crevcondissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let Some((left, right)) = children(v, t) else {
        return REG_ASSERT;
    };
    let Some(mut d) = getsubdfa(v, left) else {
        return v.err;
    };
    let Some(mut d2) = getsubdfa(v, right) else {
        putsubdfa(v, left, d);
        return v.err;
    };
    let er = crevcondissect_with(v, left, right, &mut d, &mut d2, begin, end);
    putsubdfa(v, left, d);
    putsubdfa(v, right, d2);
    er
}

#[allow(clippy::too_many_arguments)]
fn crevcondissect_with(
    v: &mut Vars,
    left: usize,
    right: usize,
    d: &mut Dfa,
    d2: &mut Dfa,
    begin: usize,
    end: usize,
) -> i32 {
    // A tentative split.
    let mid = shortest(v, d, begin, begin, end, None, None);
    if v.err != 0 {
        return v.err;
    }
    let Some(mut mid) = mid else {
        return REG_NOMATCH;
    };

    loop {
        // Whether the split works.
        if longest(v, d2, mid, end, None) == Some(end) {
            let mut er = cdissect(v, left, begin, mid);
            if er == REG_OKAY {
                er = cdissect(v, right, mid, end);
                if er == REG_OKAY {
                    return REG_OKAY;
                }
                // The left's matches cleared (the right cleared its own).
                zaptreesubs(v, left);
            }
            if er != REG_NOMATCH {
                return er;
            }
        }
        if v.err != 0 {
            return v.err;
        }

        // A new split, longer on the left.
        if mid == end {
            return REG_NOMATCH;
        }
        let next = shortest(v, d, begin, mid + 1, end, None, None);
        if v.err != 0 {
            return v.err;
        }
        match next {
            Some(m) => mid = m,
            None => return REG_NOMATCH,
        }
    }
}

/// `cbrdissect`: whether the match from `begin` to `end` is an allowed number of repetitions of
/// the string the back reference refers to.
fn cbrdissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let sub = &v.g.subres[t];
    let n = sub.backno;
    let min = sub.min as usize;
    let max = sub.max as i32;

    if n >= v.nmatch {
        return REG_ASSERT;
    }

    // The string referred to.
    if v.pmatch[n].rm_so == -1 {
        return REG_NOMATCH;
    }
    let brstring = v.pmatch[n].rm_so as usize;
    let brlen = (v.pmatch[n].rm_eo - v.pmatch[n].rm_so) as usize;

    // An empty string matches only an empty match, any number of times.
    if brlen == 0 {
        if begin == end && sub.min <= sub.max {
            return REG_OKAY;
        }
        return REG_NOMATCH;
    }
    // An empty match is no repetitions.
    if begin == end {
        if min == 0 {
            return REG_OKAY;
        }
        return REG_NOMATCH;
    }

    // Whether the length is an allowed number of repetitions.
    let tlen = end - begin;
    if !tlen.is_multiple_of(brlen) {
        return REG_NOMATCH;
    }
    let mut numreps = tlen / brlen;
    if numreps < min || (max != DUPINF && numreps > max as usize) {
        return REG_NOMATCH;
    }

    // And whether each repetition is the string.
    let mut p = begin;
    while numreps > 0 {
        if !v.g.compare(&v.string[brstring..], &v.string[p..], brlen) {
            return REG_NOMATCH;
        }
        p += brlen;
        numreps -= 1;
    }
    REG_OKAY
}

/// `caltdissect`: an alternation's match dissected: each child in turn whose DFA matches all of it,
/// until one's dissection holds.
fn caltdissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let mut child = v.g.subres[t].child;
    while let Some(c) = child {
        let Some(mut d) = getsubdfa(v, c) else {
            return v.err;
        };
        let matched = longest(v, &mut d, begin, end, None) == Some(end);
        putsubdfa(v, c, d);
        if matched {
            let er = cdissect(v, c, begin, end);
            if er != REG_NOMATCH {
                return er;
            }
        }
        if v.err != 0 {
            return v.err;
        }
        child = v.g.subres[c].sibling;
    }
    REG_NOMATCH
}

/// The places C's array of a dissection's endpoints would take, refused where that passes
/// PostgreSQL's greatest allocation.
fn endpoints(n: usize) -> Option<Vec<usize>> {
    if n.checked_mul(8).is_none_or(|b| b > MAX_ALLOC_SIZE) {
        return None;
    }
    let mut endpts = Vec::new();
    endpts.try_reserve_exact(n).ok()?;
    endpts.resize(n, 0);
    Some(endpts)
}

/// The fewest and most iterations an iteration node's dissection tries over `begin` to `end`,
/// at least one each.
fn iteration_bounds(v: &Vars, t: usize, begin: usize, end: usize) -> (usize, usize) {
    let sub = &v.g.subres[t];
    let min_matches = if sub.min <= 0 { 1 } else { sub.min as usize };
    let mut max_matches = end - begin;
    if sub.max as i32 != DUPINF && max_matches > sub.max as usize {
        max_matches = sub.max as usize;
    }
    if max_matches < min_matches {
        max_matches = min_matches;
    }
    (min_matches, max_matches)
}

/// `citerdissect`: an iteration's match dissected. Ends for the iterations are found by the
/// child's DFA, each the longest it allows, and then each iteration is dissected; where one fails,
/// the iterations are shortened from the last back. An empty match is taken for an iteration only
/// where the fewest iterations need it, and zero iterations only where nothing else matches.
fn citerdissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let Some(child) = v.g.subres[t].child else {
        return REG_ASSERT;
    };
    let (min_matches, max_matches) = iteration_bounds(v, t, begin, end);
    let Some(mut endpts) = endpoints(max_matches + 1) else {
        return REG_ESPACE;
    };
    endpts[0] = begin;

    let Some(mut d) = getsubdfa(v, child) else {
        return v.err;
    };
    let er = citerdissect_with(v, child, &mut d, &mut endpts, min_matches, max_matches, end);
    putsubdfa(v, child, d);
    if er != REG_NOMATCH {
        return er;
    }

    // Zero iterations match an empty string.
    if v.g.subres[t].min == 0 && begin == end {
        return REG_OKAY;
    }
    REG_NOMATCH
}

#[allow(clippy::too_many_arguments)]
fn citerdissect_with(
    v: &mut Vars,
    child: usize,
    d: &mut Dfa,
    endpts: &mut [usize],
    min_matches: usize,
    max_matches: usize,
    end: usize,
) -> i32 {
    let (min, max) = (min_matches as i64, max_matches as i64);
    let mut nverified: i64 = 0;
    let mut k: i64 = 1;
    let mut limit = end;

    'outer: while k > 0 {
        let ku = k as usize;
        'try_k: {
            // An end for the k'th iteration.
            let found = longest(v, d, endpts[ku - 1], limit, None);
            if v.err != 0 {
                return v.err;
            }
            let Some(e) = found else {
                // None is possible: shorten the one before.
                k -= 1;
                break 'try_k;
            };
            endpts[ku] = e;

            if nverified >= k {
                nverified = k - 1;
            }

            if e != end {
                // Not at the end yet: another iteration, where one is allowed.
                if k >= max {
                    k -= 1;
                    break 'try_k;
                }
                // An empty iteration only where the fewest need it.
                if e == endpts[ku - 1] && (k >= min || min - k < (end - e) as i64) {
                    break 'try_k;
                }
                k += 1;
                limit = end;
                continue 'outer;
            }

            // The iterations reach the end, so far as the child's DFA can tell: where there are
            // enough of them, each is dissected.
            if k < min {
                break 'try_k;
            }
            let mut i = nverified + 1;
            while i <= k {
                let iu = i as usize;
                zaptreesubs(v, child);
                let er = cdissect(v, child, endpts[iu - 1], endpts[iu]);
                if er == REG_OKAY {
                    nverified = i;
                    i += 1;
                    continue;
                }
                if er == REG_NOMATCH {
                    break;
                }
                return er;
            }
            if i > k {
                return REG_OKAY;
            }

            // The i'th failed: it is backtracked.
            k = i;
        }

        // A shorter k'th iteration, an empty one only where needed; else the one before.
        while k > 0 {
            let ku = k as usize;
            let prev_end = endpts[ku - 1];
            if endpts[ku] > prev_end {
                limit = endpts[ku] - 1;
                if limit > prev_end || (k < min && min - k >= (end - prev_end) as i64) {
                    break;
                }
            }
            k -= 1;
        }
    }

    REG_NOMATCH
}

/// `creviterdissect`: an iteration's match dissected where its child prefers the shortest match:
/// as `citerdissect`, each iteration's end the shortest the child's DFA allows, lengthened from
/// the last back where an iteration fails.
fn creviterdissect(v: &mut Vars, t: usize, begin: usize, end: usize) -> i32 {
    let Some(child) = v.g.subres[t].child else {
        return REG_ASSERT;
    };

    // Zero iterations match an empty string; a string that is not empty needs one at least.
    if v.g.subres[t].min <= 0 && begin == end {
        return REG_OKAY;
    }
    let (min_matches, max_matches) = iteration_bounds(v, t, begin, end);
    let Some(mut endpts) = endpoints(max_matches + 1) else {
        return REG_ESPACE;
    };
    endpts[0] = begin;

    let Some(mut d) = getsubdfa(v, child) else {
        return v.err;
    };
    let er = creviterdissect_with(
        v,
        child,
        &mut d,
        &mut endpts,
        min_matches,
        max_matches,
        begin,
        end,
    );
    putsubdfa(v, child, d);
    er
}

#[allow(clippy::too_many_arguments)]
fn creviterdissect_with(
    v: &mut Vars,
    child: usize,
    d: &mut Dfa,
    endpts: &mut [usize],
    min_matches: usize,
    max_matches: usize,
    begin: usize,
    end: usize,
) -> i32 {
    let (min, max) = (min_matches as i64, max_matches as i64);
    let mut nverified: i64 = 0;
    let mut k: i64 = 1;
    let mut limit = begin;

    'outer: while k > 0 {
        let ku = k as usize;
        'try_k: {
            // An empty iteration only where the fewest need it.
            if limit == endpts[ku - 1]
                && limit != end
                && (k >= min || min - k < (end - limit) as i64)
            {
                limit += 1;
            }

            // The last iteration allowed reaches the end.
            if k >= max {
                limit = end;
            }

            // An end for the k'th iteration.
            let found = shortest(v, d, endpts[ku - 1], limit, end, None, None);
            if v.err != 0 {
                return v.err;
            }
            let Some(e) = found else {
                // None is possible: lengthen the one before.
                k -= 1;
                break 'try_k;
            };
            endpts[ku] = e;

            if nverified >= k {
                nverified = k - 1;
            }

            if e != end {
                // Not at the end yet: another iteration, where one is allowed.
                if k >= max {
                    k -= 1;
                    break 'try_k;
                }
                k += 1;
                limit = endpts[ku];
                continue 'outer;
            }

            // The iterations reach the end, so far as the child's DFA can tell: where there are
            // enough of them, each is dissected.
            if k < min {
                break 'try_k;
            }
            let mut i = nverified + 1;
            while i <= k {
                let iu = i as usize;
                zaptreesubs(v, child);
                let er = cdissect(v, child, endpts[iu - 1], endpts[iu]);
                if er == REG_OKAY {
                    nverified = i;
                    i += 1;
                    continue;
                }
                if er == REG_NOMATCH {
                    break;
                }
                return er;
            }
            if i > k {
                return REG_OKAY;
            }

            // The i'th failed: it is backtracked.
            k = i;
        }

        // A longer k'th iteration; else the one before.
        while k > 0 {
            let ku = k as usize;
            if endpts[ku] < end {
                limit = endpts[ku] + 1;
                break;
            }
            k -= 1;
        }
    }

    REG_NOMATCH
}

#[cfg(test)]
mod tests {
    //! Compiled patterns built by hand, as the compiler lays them out, each run through the
    //! executor.

    use super::*;
    use crate::regex::guts::{
        Carc, Cnfa, Color, ColorMap, Subre, BACKR, BRUSE, CAP, CNFA_NOPROGRESS, COLORLESS,
        HASLACONS, LATYPE_AHEAD_POS, LATYPE_BEHIND_NEG, LONGER, MATCHALL, MAX_SIMPLE_CHR, MIXED,
        NUM_CCLASSES, RAINBOW, REG_ADVANCED, REG_ICASE, REG_NOTBOL, REG_NOTEOL, REG_SMALL,
        REG_ULOOKAROUND, WHITE,
    };
    use crate::regex::locale::Ctype;

    // The colors: the pseudocolors of the string's edges, then a, b and c.
    const BOS0: Color = 1;
    const BOS1: Color = 2;
    const EOS0: Color = 3;
    const EOS1: Color = 4;
    const A: Color = 5;
    const B: Color = 6;
    const C: Color = 7;
    const NCOLORS: usize = 8;
    /// The "color" of lookaround constraint `n`.
    fn lacon(n: usize) -> Color {
        (NCOLORS + n) as Color
    }

    const POST: usize = 0;
    const PRE: usize = 1;

    /// The colors of a, b and c, and of A where the pattern ignores case.
    fn cmap(icase: bool) -> ColorMap {
        let mut lo = vec![WHITE; MAX_SIMPLE_CHR as usize + 1];
        lo[b'a' as usize] = A;
        lo[b'b' as usize] = B;
        lo[b'c' as usize] = C;
        if icase {
            lo[b'A' as usize] = A;
        }
        ColorMap {
            locolormap: lo,
            classbits: [0; NUM_CCLASSES],
            cmranges: vec![],
            hicolormap: vec![WHITE],
            hiarraycols: 1,
            pseudo: (0..NCOLORS).map(|c| (1..=4).contains(&c)).collect(),
            ctype: Ctype::C,
        }
    }

    /// An NFA by its arcs: state 0 the post state, 1 the pre state.
    #[derive(Clone, Debug)]
    struct Nfa {
        nstates: usize,
        arcs: Vec<(usize, Color, usize)>,
        flags: i32,
        minmatchall: i32,
        maxmatchall: i32,
    }

    fn nfa(nstates: usize, arcs: &[(usize, Color, usize)]) -> Nfa {
        Nfa {
            nstates,
            arcs: arcs.to_vec(),
            flags: 0,
            minmatchall: -1,
            maxmatchall: -1,
        }
    }

    /// The arcs out of the pre state, unanchored, and into the post state.
    fn edges(arcs: &mut Vec<(usize, Color, usize)>, first: usize, last: usize) {
        for co in [RAINBOW, BOS0, BOS1] {
            arcs.push((PRE, co, first));
        }
        for co in [RAINBOW, EOS0, EOS1] {
            arcs.push((last, co, POST));
        }
    }

    /// The NFA of a sequence of steps, each one character of a set of colors.
    fn chain(steps: &[&[Color]]) -> Nfa {
        let mut arcs = vec![];
        for (i, step) in steps.iter().enumerate() {
            for &co in *step {
                arcs.push((2 + i, co, 3 + i));
            }
        }
        edges(&mut arcs, 2, 2 + steps.len());
        nfa(3 + steps.len(), &arcs)
    }

    /// The NFA of `x*` (`plus` false) or `x+` (`plus` true) for a set of colors `x`.
    fn star(cos: &[Color], plus: bool) -> Nfa {
        let mut arcs = vec![];
        let last = if plus { 3 } else { 2 };
        for &co in cos {
            arcs.push((2, co, last));
            arcs.push((last, co, last));
        }
        edges(&mut arcs, 2, last);
        nfa(last + 1, &arcs)
    }

    /// The compacted NFA, laid out as `compact` lays it out.
    fn compact(n: &Nfa) -> Cnfa {
        let mut states = vec![];
        let mut arcs = vec![];
        for s in 0..n.nstates {
            states.push(arcs.len());
            let mut outs: Vec<Carc> = (n.arcs.iter())
                .filter(|a| a.0 == s)
                .map(|a| Carc { co: a.1, to: a.2 })
                .collect();
            outs.sort_by_key(|a| (a.co, a.to));
            outs.dedup();
            arcs.extend(outs);
            arcs.push(Carc {
                co: COLORLESS,
                to: 0,
            });
        }
        let mut stflags = vec![0; n.nstates];
        for a in n.arcs.iter().filter(|a| a.0 == PRE) {
            stflags[a.2] = CNFA_NOPROGRESS;
        }
        stflags[PRE] = CNFA_NOPROGRESS;
        let haslacons = n.arcs.iter().any(|a| a.1 as usize >= NCOLORS);
        Cnfa {
            nstates: n.nstates,
            ncolors: NCOLORS,
            flags: n.flags | if haslacons { HASLACONS } else { 0 },
            pre: PRE,
            post: POST,
            bos: [BOS0, BOS1],
            eos: [EOS0, EOS1],
            stflags,
            states,
            arcs,
            minmatchall: n.minmatchall,
            maxmatchall: n.maxmatchall,
        }
    }

    /// The search NFA of an NFA, as `makesearch` makes it: a loop on the pre state where it is
    /// not anchored, and each state the pre state reaches that is reached from elsewhere too split
    /// in two.
    fn makesearch(n: &Nfa) -> Nfa {
        let mut n = n.clone();
        let anchored = (n.arcs.iter())
            .filter(|a| a.0 == PRE)
            .all(|a| a.1 == BOS0 || a.1 == BOS1);
        if !anchored {
            for co in [RAINBOW, BOS0, BOS1] {
                n.arcs.push((PRE, co, PRE));
            }
            if n.flags & MATCHALL != 0 {
                n.maxmatchall = DUPINF;
            }
        }
        let mut targets = vec![];
        for a in n.arcs.iter().filter(|a| a.0 == PRE) {
            if !targets.contains(&a.2) {
                targets.push(a.2);
            }
        }
        for s in targets {
            if !n.arcs.iter().any(|a| a.2 == s && a.0 != PRE) {
                continue;
            }
            let s2 = n.nstates;
            n.nstates += 1;
            let outs: Vec<_> = (n.arcs.iter())
                .filter(|a| a.0 == s)
                .map(|a| (s2, a.1, a.2))
                .collect();
            n.arcs.extend(outs);
            for a in n.arcs.iter_mut() {
                if a.2 == s && a.0 != PRE {
                    a.2 = s2;
                }
            }
        }
        n
    }

    fn node(op: u8, flags: u8, n: &Nfa) -> Subre {
        Subre {
            op,
            flags,
            cnfa: compact(n),
            ..Subre::default()
        }
    }

    /// A compiled pattern from its tree, by id from 1, and the whole pattern's NFA.
    fn guts(mut tree: Vec<Subre>, whole: &Nfa, nsub: usize, info: i64, cflags: i32) -> Guts {
        let mut subres = vec![Subre::default()];
        subres.append(&mut tree);
        for (id, s) in subres.iter_mut().enumerate() {
            s.id = id;
        }
        subres[1].cnfa = compact(whole);
        Guts {
            cflags,
            info,
            nsub,
            ntree: subres.len(),
            subres,
            search: compact(&makesearch(whole)),
            cmap: cmap(cflags & REG_ICASE != 0),
            lacons: vec![],
            ctype: Ctype::C,
        }
    }

    fn chars(s: &str) -> Vec<Chr> {
        s.chars().map(|c| c as Chr).collect()
    }

    fn run_from(
        g: &Guts,
        s: &str,
        from: usize,
        nmatch: usize,
        eflags: i32,
    ) -> Option<Vec<(i64, i64)>> {
        exec(g, &chars(s), from, nmatch, eflags, None)
            .unwrap()
            .map(|m| m.iter().map(|m| (m.rm_so, m.rm_eo)).collect())
    }

    fn run(g: &Guts, s: &str, nmatch: usize) -> Option<Vec<(i64, i64)>> {
        run_from(g, s, 0, nmatch, 0)
    }

    /// `a`.
    fn plain_a() -> Guts {
        let a = chain(&[&[A]]);
        guts(vec![node(b'=', 0, &a)], &a, 0, 0, REG_ADVANCED)
    }

    #[test]
    fn a_plain_pattern_is_found_by_its_dfas_alone() {
        let g = plain_a();
        assert_eq!(run(&g, "xxaxx", 1), Some(vec![(2, 3)]));
        assert_eq!(run(&g, "xxaxx", 0), Some(vec![]));
        assert_eq!(run(&g, "xxaxx", 3), Some(vec![(2, 3), (-1, -1), (-1, -1)]));
        assert_eq!(run(&g, "bbb", 1), None);
        assert_eq!(run(&g, "", 1), None);
        assert_eq!(run_from(&g, "axa", 1, 1, 0), Some(vec![(2, 3)]));
        assert_eq!(run_from(&g, "axa", 3, 1, 0), None);
        assert_eq!(run_from(&g, "axa", 4, 1, 0), None);
    }

    #[test]
    fn an_impossible_pattern_and_a_missing_details_refuse() {
        let mut g = plain_a();
        g.info |= REG_UIMPOSSIBLE;
        assert_eq!(run(&g, "a", 1), None);
        let mut g = plain_a();
        g.cflags |= REG_EXPECT;
        assert_eq!(
            exec(&g, &chars("a"), 0, 1, 0, None),
            Err(RegError(REG_INVARG))
        );
    }

    #[test]
    fn expect_gives_where_a_longer_match_could_still_start() {
        let mut g = plain_a();
        g.cflags |= REG_EXPECT;
        let mut details = RegMatch::UNSET;
        let m = exec(&g, &chars("xxa"), 0, 1, 0, Some(&mut details)).unwrap();
        assert_eq!(m, Some(vec![RegMatch { rm_so: 2, rm_eo: 3 }]));
        assert_eq!(details, RegMatch { rm_so: 2, rm_eo: 3 });
        let m = exec(&g, &chars("xxax"), 0, 1, 0, Some(&mut details)).unwrap();
        assert_eq!(m, Some(vec![RegMatch { rm_so: 2, rm_eo: 3 }]));
        assert_eq!(details, RegMatch { rm_so: 2, rm_eo: 4 });
        let m = exec(&g, &chars("xx"), 0, 1, 0, Some(&mut details)).unwrap();
        assert_eq!(m, None);
        assert_eq!(details, RegMatch { rm_so: 2, rm_eo: 2 });
    }

    #[test]
    fn the_string_edges_follow_notbol_and_noteol() {
        // ^a: only the start of the string, where it is a line's.
        let mut arcs = vec![(PRE, BOS1, 2), (2, A, 3)];
        for co in [RAINBOW, EOS0, EOS1] {
            arcs.push((3, co, POST));
        }
        let anchored = nfa(4, &arcs);
        let g = guts(vec![node(b'=', 0, &anchored)], &anchored, 0, 0, 0);
        assert_eq!(run(&g, "ab", 1), Some(vec![(0, 1)]));
        assert_eq!(run(&g, "ba", 1), None);
        assert_eq!(run_from(&g, "ab", 0, 1, REG_NOTBOL), None);

        // a$: only the end of the string, where it is a line's.
        let mut arcs = vec![(2, A, 3), (3, EOS1, POST)];
        for co in [RAINBOW, BOS0, BOS1] {
            arcs.push((PRE, co, 2));
        }
        let anchored = nfa(4, &arcs);
        let g = guts(vec![node(b'=', 0, &anchored)], &anchored, 0, 0, 0);
        assert_eq!(run(&g, "aba", 1), Some(vec![(2, 3)]));
        assert_eq!(run(&g, "ab", 1), None);
        assert_eq!(run_from(&g, "ba", 0, 1, REG_NOTEOL), None);
    }

    #[test]
    fn a_matchall_pattern_takes_its_fast_paths() {
        // .* and .{2}
        let mut any = star(&[RAINBOW], false);
        any.flags = MATCHALL;
        any.minmatchall = 0;
        any.maxmatchall = DUPINF;
        let g = guts(vec![node(b'=', 0, &any)], &any, 0, 0, 0);
        assert_eq!(run(&g, "abc", 1), Some(vec![(0, 3)]));
        assert_eq!(run_from(&g, "abc", 1, 1, 0), Some(vec![(1, 3)]));
        assert_eq!(run(&g, "", 1), Some(vec![(0, 0)]));

        let mut two = chain(&[&[RAINBOW], &[RAINBOW]]);
        two.flags = MATCHALL;
        two.minmatchall = 2;
        two.maxmatchall = 2;
        let g = guts(vec![node(b'=', 0, &two)], &two, 0, 0, 0);
        assert_eq!(run(&g, "abc", 1), Some(vec![(0, 2)]));
        assert_eq!(run(&g, "a", 1), None);
    }

    /// (a)(b)
    fn two_captures(cflags: i32) -> Guts {
        let tree = vec![
            Subre {
                child: Some(2),
                ..node(b'.', CAP, &chain(&[&[A], &[B]]))
            },
            Subre {
                capno: 1,
                sibling: Some(3),
                ..node(b'=', CAP, &chain(&[&[A]]))
            },
            Subre {
                capno: 2,
                ..node(b'=', CAP, &chain(&[&[B]]))
            },
        ];
        guts(tree, &chain(&[&[A], &[B]]), 2, 0, cflags)
    }

    #[test]
    fn a_concatenation_is_dissected_into_its_captures() {
        let g = two_captures(0);
        assert_eq!(run(&g, "xab", 3), Some(vec![(1, 3), (1, 2), (2, 3)]));
        assert_eq!(run(&g, "xab", 2), Some(vec![(1, 3), (1, 2)]));
        assert_eq!(run(&g, "xab", 1), Some(vec![(1, 3)]));
        assert_eq!(
            run(&g, "xab", 5),
            Some(vec![(1, 3), (1, 2), (2, 3), (-1, -1), (-1, -1)])
        );
        assert_eq!(run(&g, "xa", 3), None);

        // Where no captures are wanted, none are shown.
        let g = two_captures(REG_NOSUB);
        assert_eq!(run(&g, "xab", 3), Some(vec![(1, 3), (-1, -1), (-1, -1)]));
    }

    #[test]
    fn an_alternation_takes_the_first_child_that_matches() {
        // (a)|(b)
        let whole = chain(&[&[A, B]]);
        let tree = vec![
            Subre {
                child: Some(2),
                ..node(b'|', CAP, &whole)
            },
            Subre {
                capno: 1,
                sibling: Some(3),
                ..node(b'=', CAP, &chain(&[&[A]]))
            },
            Subre {
                capno: 2,
                ..node(b'=', CAP, &chain(&[&[B]]))
            },
        ];
        let g = guts(tree, &whole, 2, 0, 0);
        assert_eq!(run(&g, "xb", 3), Some(vec![(1, 2), (-1, -1), (1, 2)]));
        assert_eq!(run(&g, "ab", 3), Some(vec![(0, 1), (0, 1), (-1, -1)]));
    }

    /// ([ab])\1, or (a)\1 where `only_a`.
    fn backref(only_a: bool, cflags: i32) -> Guts {
        let cls: &[Color] = if only_a { &[A] } else { &[A, B] };
        let whole = chain(&[cls, cls]);
        let tree = vec![
            Subre {
                child: Some(2),
                ..node(b'.', CAP | BACKR, &whole)
            },
            Subre {
                capno: 1,
                sibling: Some(3),
                ..node(b'=', CAP | BRUSE, &chain(&[cls]))
            },
            Subre {
                backno: 1,
                min: 1,
                max: 1,
                ..node(b'b', BACKR, &chain(&[cls]))
            },
        ];
        guts(tree, &whole, 1, REG_UBACKREF, cflags)
    }

    #[test]
    fn a_back_reference_is_checked_against_what_it_refers_to() {
        let g = backref(false, 0);
        assert_eq!(run(&g, "abba", 2), Some(vec![(1, 3), (1, 2)]));
        assert_eq!(run(&g, "abba", 1), Some(vec![(1, 3)]));
        assert_eq!(run(&g, "abba", 0), Some(vec![]));
        assert_eq!(run(&g, "abab", 2), None);
        assert_eq!(run(&g, "xaa", 3), Some(vec![(1, 3), (1, 2), (-1, -1)]));

        let g = backref(true, 0);
        assert_eq!(run(&g, "aA", 2), None);
        let g = backref(true, REG_ICASE);
        assert_eq!(run(&g, "aA", 2), Some(vec![(0, 2), (0, 1)]));
    }

    /// (a)* or, with bounds, (a){min,max}.
    fn iteration(min: i16, max: i16, whole: &Nfa) -> Guts {
        let tree = vec![
            Subre {
                min,
                max,
                child: Some(2),
                ..node(b'*', CAP | LONGER, whole)
            },
            Subre {
                capno: 1,
                ..node(b'=', CAP, &chain(&[&[A]]))
            },
        ];
        guts(tree, whole, 1, 0, 0)
    }

    #[test]
    fn an_iteration_captures_its_last_repetition() {
        let g = iteration(0, DUPINF as i16, &star(&[A], false));
        assert_eq!(run(&g, "aaa", 2), Some(vec![(0, 3), (2, 3)]));
        assert_eq!(run(&g, "baaa", 2), Some(vec![(0, 0), (-1, -1)]));
        assert_eq!(run_from(&g, "baaa", 1, 2, 0), Some(vec![(1, 4), (3, 4)]));

        // (a){2,3}
        let mut arcs = vec![(2, A, 3), (3, A, 4), (4, A, 5)];
        for co in [RAINBOW, BOS0, BOS1] {
            arcs.push((PRE, co, 2));
        }
        for s in [4, 5] {
            for co in [RAINBOW, EOS0, EOS1] {
                arcs.push((s, co, POST));
            }
        }
        let g = iteration(2, 3, &nfa(6, &arcs));
        assert_eq!(run(&g, "aaaa", 2), Some(vec![(0, 3), (2, 3)]));
        assert_eq!(run(&g, "xaax", 2), Some(vec![(1, 3), (2, 3)]));
        assert_eq!(run(&g, "xax", 2), None);
    }

    #[test]
    fn a_shortest_left_child_splits_from_the_left() {
        // (a+?)(a*): the whole pattern prefers the shortest match.
        let whole = star(&[A], true);
        let tree = vec![
            Subre {
                child: Some(2),
                ..node(b'.', SHORTER | CAP | MIXED, &whole)
            },
            Subre {
                capno: 1,
                sibling: Some(3),
                ..node(b'=', SHORTER | CAP, &star(&[A], true))
            },
            Subre {
                capno: 2,
                ..node(b'=', LONGER | CAP, &star(&[A], false))
            },
        ];
        let g = guts(tree, &whole, 2, 0, 0);
        assert_eq!(run(&g, "aaa", 3), Some(vec![(0, 1), (0, 1), (1, 1)]));
    }

    #[test]
    fn a_shortest_child_iterates_from_the_shortest() {
        // (a+?)+ over a+, the iteration's match the longest.
        let whole = star(&[A], true);
        let tree = vec![
            Subre {
                min: 1,
                max: DUPINF as i16,
                child: Some(2),
                ..node(b'*', LONGER | CAP | MIXED, &whole)
            },
            Subre {
                capno: 1,
                ..node(b'=', SHORTER | CAP, &star(&[A], true))
            },
        ];
        let g = guts(tree, &whole, 1, 0, 0);
        assert_eq!(run(&g, "aaa", 2), Some(vec![(0, 3), (2, 3)]));
        assert_eq!(run(&g, "xaab", 2), Some(vec![(1, 3), (2, 3)]));
    }

    #[test]
    fn lookaround_constraints_are_tested_where_they_stand() {
        // a(?=b)
        let mut arcs = vec![(2, A, 3), (3, lacon(1), 4)];
        edges(&mut arcs, 2, 4);
        let whole = nfa(5, &arcs);
        let mut g = guts(vec![node(b'=', 0, &whole)], &whole, 0, REG_ULOOKAROUND, 0);
        g.lacons = vec![
            Subre::default(),
            Subre {
                latype: LATYPE_AHEAD_POS,
                ..node(b'=', 0, &chain(&[&[B]]))
            },
        ];
        assert_eq!(run(&g, "ab", 1), Some(vec![(0, 1)]));
        assert_eq!(run(&g, "ac", 1), None);
        assert_eq!(run(&g, "acab", 1), Some(vec![(2, 3)]));

        // (?<!a)b
        let mut arcs = vec![(2, lacon(1), 3), (3, B, 4)];
        edges(&mut arcs, 2, 4);
        let whole = nfa(5, &arcs);
        let mut g = guts(vec![node(b'=', 0, &whole)], &whole, 0, REG_ULOOKAROUND, 0);
        g.lacons = vec![
            Subre::default(),
            Subre {
                latype: LATYPE_BEHIND_NEG,
                ..node(b'=', 0, &makesearch(&chain(&[&[A]])))
            },
        ];
        assert_eq!(run(&g, "ab", 1), None);
        assert_eq!(run(&g, "cb", 1), Some(vec![(1, 2)]));
        assert_eq!(run(&g, "abcb", 1), Some(vec![(3, 4)]));
        assert_eq!(run(&g, "b", 1), Some(vec![(0, 1)]));
    }

    /// [ab]*a[ab][ab], whose DFA has more state sets than a small cache holds.
    fn third_from_last() -> Guts {
        let mut arcs = vec![(2, A, 2), (2, B, 2), (2, A, 3)];
        for (s, t) in [(3, 4), (4, 5)] {
            arcs.push((s, A, t));
            arcs.push((s, B, t));
        }
        edges(&mut arcs, 2, 5);
        let whole = nfa(6, &arcs);
        guts(vec![node(b'=', 0, &whole)], &whole, 0, 0, 0)
    }

    /// Where `[ab]*a[ab][ab]` first matches, leftmost and then longest.
    fn third_from_last_by_hand(s: &[u8]) -> Option<(i64, i64)> {
        for begin in 0..s.len() {
            let mut best = None;
            for end in begin + 3..=s.len() {
                let m = &s[begin..end];
                if m.iter().all(|&c| c == b'a' || c == b'b') && m[m.len() - 3] == b'a' {
                    best = Some((begin as i64, end as i64));
                }
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }

    #[test]
    fn a_small_cache_replaces_its_state_sets_and_finds_the_same_matches() {
        let g = third_from_last();
        let mut seed: u64 = 0x2545_f491_4f6c_dd1d;
        for _ in 0..400 {
            let len = (seed % 40) as usize;
            let s: Vec<u8> = (0..len)
                .map(|_| {
                    seed ^= seed << 13;
                    seed ^= seed >> 7;
                    seed ^= seed << 17;
                    b"ababx"[(seed % 5) as usize]
                })
                .collect();
            let text = std::str::from_utf8(&s).unwrap();
            let want = third_from_last_by_hand(&s).map(|m| vec![m]);
            assert_eq!(run(&g, text, 1), want, "{text}");
            assert_eq!(run_from(&g, text, 0, 1, REG_SMALL), want, "{text} small");
        }
    }

    /// A tree of concatenations `depth` deep, each an empty left child and the next to its right,
    /// the last a capture of `a`.
    fn deep(depth: usize) -> Guts {
        let a = chain(&[&[A]]);
        let empty = chain(&[]);
        let mut tree = vec![];
        for level in 0..depth {
            let id = tree.len() + 1;
            tree.push(Subre {
                child: Some(id + 1),
                ..node(b'.', CAP, &a)
            });
            tree.push(Subre {
                sibling: Some(id + 2),
                ..node(b'=', 0, &empty)
            });
            if level == depth - 1 {
                tree.push(Subre {
                    capno: 1,
                    ..node(b'=', CAP, &a)
                });
            }
        }
        guts(tree, &a, 1, 0, 0)
    }

    #[test]
    fn a_tree_too_deep_is_refused_and_one_just_shallow_enough_runs_on_a_small_stack() {
        let g = deep(MAX_DEPTH / DISSECT_DEPTH + 10);
        assert_eq!(
            exec(&g, &chars("xa"), 0, 2, 0, None),
            Err(RegError(REG_ETOOBIG))
        );
        let found = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let g = deep(MAX_DEPTH / DISSECT_DEPTH - 1);
                run(&g, "xa", 2)
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(found, Some(vec![(1, 2), (1, 2)]));
    }

    /// `depth` lookaheads nested, each wrapping the next, the last `b`: a(?=(?=...(?=b)...)).
    fn nested_lookaheads(depth: usize) -> Guts {
        let mut arcs = vec![(2, A, 3), (3, lacon(1), 4)];
        edges(&mut arcs, 2, 4);
        let whole = nfa(5, &arcs);
        let mut g = guts(vec![node(b'=', 0, &whole)], &whole, 0, REG_ULOOKAROUND, 0);
        g.lacons = vec![Subre::default()];
        for n in 1..=depth {
            let inner = if n == depth {
                chain(&[&[B]])
            } else {
                let mut arcs = vec![(2, lacon(n + 1), 3)];
                edges(&mut arcs, 2, 3);
                nfa(4, &arcs)
            };
            g.lacons.push(Subre {
                latype: LATYPE_AHEAD_POS,
                ..node(b'=', 0, &inner)
            });
        }
        g
    }

    #[test]
    fn lookaheads_nested_too_deep_are_refused_and_shallow_enough_run_on_a_small_stack() {
        let g = nested_lookaheads(MAX_DEPTH / LACON_DEPTH + 10);
        assert_eq!(
            exec(&g, &chars("ab"), 0, 1, 0, None),
            Err(RegError(REG_ETOOBIG))
        );
        let found = std::thread::Builder::new()
            .stack_size(2 << 20)
            .spawn(|| {
                let g = nested_lookaheads(MAX_DEPTH / LACON_DEPTH - 1);
                (run(&g, "ab", 1), run(&g, "ac", 1))
            })
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(found, (Some(vec![(0, 1)]), None));
    }
}
