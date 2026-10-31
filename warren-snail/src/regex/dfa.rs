//! The lazy DFA (rege_dfa.c): a compacted NFA run over the string one character at a time, each
//! set of NFA states it can be in made into a DFA state the first time it is reached and kept in a
//! cache of fixed size, as PostgreSQL 19's executor runs it.

use super::exec::{Vars, LACON_DEPTH};
use super::guts::{
    latype_is_ahead, latype_is_pos, Cnfa, Color, ColorMap, CNFA_NOPROGRESS, COLORLESS, DUPINF,
    HASLACONS, MATCHALL, RAINBOW, REG_ASSERT, REG_ESPACE, REG_ETOOBIG, REG_NOTBOL, REG_NOTEOL,
    REG_SMALL, WHITE,
};

// A state set's flags.
const STARTER: i32 = 0o1;
const POSTSTATE: i32 = 0o2;
const LOCKED: i32 = 0o4;
const NOPROGRESS: i32 = 0o10;

/// The work bitvectors a DFA keeps beside its state sets.
const WORK: usize = 1;
/// A DFA of at most this many state sets and colors is C's small case (`struct smalldfa`).
const FEWSTATES: usize = 20;
const FEWCOLORS: usize = 15;
/// The bits in one word of a state set's bitvector (`UBITS`).
const UBITS: usize = 32;
/// The greatest allocation PostgreSQL makes (`MaxAllocSize`).
const MAX_ALLOC_SIZE: usize = 0x3fff_ffff;
// The sizes of C's arrays' elements on a 64-bit machine: a `struct sset`, an `unsigned`, a
// pointer, a `struct arcp`.
const SIZEOF_SSET: usize = 56;
const SIZEOF_UNSIGNED: usize = 4;
const SIZEOF_POINTER: usize = 8;
const SIZEOF_ARCP: usize = 16;

/// No state set.
const NONE: u32 = u32::MAX;

/// An arc of the DFA, by the state set it leaves and its color (`struct arcp`).
#[derive(Clone, Copy, Debug)]
struct Arcp {
    ss: u32,
    co: Color,
}

impl Arcp {
    const NULL: Arcp = Arcp {
        ss: NONE,
        co: WHITE,
    };
}

/// One state set of the cache (`struct sset`). Its bitvector lives in `Dfa::statesarea`, its arcs
/// out by color in `Dfa::outsarea` and its links in the chains of arcs in in `Dfa::incarea`, each
/// at its slot's place.
#[derive(Clone, Copy, Debug)]
struct Sset {
    hash: u32,
    flags: i32,
    /// The first of the arcs that lead here.
    ins: Arcp,
    /// Where the scan last arrived here.
    lastseen: Option<usize>,
}

/// A lazy DFA over one compacted NFA (`struct dfa`).
pub(super) struct Dfa<'a> {
    /// The cache's size.
    nssets: usize,
    /// How many of its slots are in use yet.
    nssused: usize,
    nstates: usize,
    ncolors: usize,
    /// The words of a state set's bitvector.
    wordsper: usize,
    ssets: Vec<Sset>,
    statesarea: Vec<u32>,
    work: Vec<u32>,
    outsarea: Vec<u32>,
    incarea: Vec<Arcp>,
    cnfa: &'a Cnfa,
    cm: &'a ColorMap,
    /// Where the last success the cache forgot was.
    lastpost: Option<usize>,
    /// Where the last no-progress state set the cache forgot was.
    lastnopr: Option<usize>,
    /// Where the search for a slot to replace starts.
    search: usize,
    /// For a back reference's DFA, the subexpression it refers to, else -1; its fewest and most
    /// repetitions.
    pub(super) backno: i32,
    pub(super) backmin: i16,
    pub(super) backmax: i16,
}

fn bset(uv: &mut [u32], sn: usize) {
    uv[sn / UBITS] |= 1u32 << (sn % UBITS);
}

fn isbset(uv: &[u32], sn: usize) -> bool {
    uv[sn / UBITS] & (1u32 << (sn % UBITS)) != 0
}

/// The color a scan starting at `cp` starts on: BOS (or BOL) at the string's start, else the color
/// of the character before.
fn startcolor(v: &Vars, d: &Dfa, cp: usize) -> Color {
    if cp == 0 {
        d.cnfa.bos[if v.eflags & REG_NOTBOL != 0 { 0 } else { 1 }]
    } else {
        d.cm.getcolor(v.string[cp - 1])
    }
}

/// The color of the string's end: EOS, or EOL.
fn eoscolor(v: &Vars, d: &Dfa) -> Color {
    d.cnfa.eos[if v.eflags & REG_NOTEOL != 0 { 0 } else { 1 }]
}

impl Dfa<'_> {
    fn outs(&self, ss: usize, co: Color) -> u32 {
        self.outsarea[ss * self.ncolors + co as usize]
    }

    fn flags(&self, ss: usize) -> i32 {
        self.ssets[ss].flags
    }

    fn states(&self, ss: usize) -> &[u32] {
        &self.statesarea[ss * self.wordsper..(ss + 1) * self.wordsper]
    }
}

/// `longest`: the end of the longest match of the DFA that starts at `start` and ends at or before
/// `stop`; None where there is none, or on an error (`v.err` set). `hitstopp`, where given, says
/// whether the scan reached the string's end.
pub(super) fn longest(
    v: &mut Vars,
    d: &mut Dfa,
    start: usize,
    stop: usize,
    mut hitstopp: Option<&mut bool>,
) -> Option<usize> {
    let realstop = if stop == v.stop { stop } else { stop + 1 };
    let cm = d.cm;

    if let Some(h) = hitstopp.as_deref_mut() {
        *h = false;
    }

    // A back reference to a string already matched is matched against that string.
    if d.backno >= 0 && v.pmatch[d.backno as usize].rm_so >= 0 {
        let cp = dfa_backref(v, d, start, start, stop, false);
        if cp == Some(v.stop) && stop == v.stop {
            if let Some(h) = hitstopp {
                *h = true;
            }
        }
        return cp;
    }

    // An NFA that matches every string of a range of lengths.
    if d.cnfa.flags & MATCHALL != 0 {
        let nchr = (stop - start) as i64;
        let maxmatchall = d.cnfa.maxmatchall as i64;

        if nchr < d.cnfa.minmatchall as i64 {
            return None;
        }
        if maxmatchall == DUPINF as i64 {
            if stop == v.stop {
                if let Some(h) = hitstopp {
                    *h = true;
                }
            }
        } else {
            if stop == v.stop && nchr <= maxmatchall + 1 {
                if let Some(h) = hitstopp {
                    *h = true;
                }
            }
            if nchr > maxmatchall {
                return Some(start + maxmatchall as usize);
            }
        }
        return Some(stop);
    }

    let mut css = initialize(v, d, start)?;
    let mut cp = start;

    // The startup.
    let co = startcolor(v, d, cp);
    css = miss(v, d, css, co, cp, start)?;
    d.ssets[css].lastseen = Some(cp);

    // The scan.
    while cp < realstop {
        let co = cm.getcolor(v.string[cp]);
        let mut ss = d.outs(css, co);
        if ss == NONE {
            match miss(v, d, css, co, cp + 1, start) {
                Some(s) => ss = s as u32,
                None => break,
            }
        }
        cp += 1;
        let ss = ss as usize;
        d.ssets[ss].lastseen = Some(cp);
        css = ss;
    }

    if v.err != 0 {
        return None;
    }

    // The shutdown.
    if cp == v.stop && stop == v.stop {
        if let Some(h) = hitstopp {
            *h = true;
        }
        let co = eoscolor(v, d);
        let ss = miss(v, d, css, co, cp, start);
        if v.err != 0 {
            return None;
        }
        if let Some(ss) = ss {
            // The match ended at the string's end.
            if d.flags(ss) & POSTSTATE != 0 {
                return Some(cp);
            }
            d.ssets[ss].lastseen = Some(cp);
        }
    }

    // The last match, if any.
    let mut post = d.lastpost;
    for ss in &d.ssets[..d.nssused] {
        if ss.flags & POSTSTATE != 0
            && post != ss.lastseen
            && (post.is_none() || post < ss.lastseen)
        {
            post = ss.lastseen;
        }
    }
    post.map(|p| p - 1)
}

/// `shortest`: the end of the shortest match of the DFA that starts at `start` and ends between
/// `min` and `max`; None where there is none, or on an error (`v.err` set). `coldp`, where given,
/// gets the last place where no progress had been made; `hitstopp`, where given, says whether the
/// scan reached the string's end without a match.
pub(super) fn shortest(
    v: &mut Vars,
    d: &mut Dfa,
    start: usize,
    mut min: usize,
    max: usize,
    mut coldp: Option<&mut Option<usize>>,
    mut hitstopp: Option<&mut bool>,
) -> Option<usize> {
    let realmin = if min == v.stop { min } else { min + 1 };
    let realmax = if max == v.stop { max } else { max + 1 };
    let cm = d.cm;

    if let Some(c) = coldp.as_deref_mut() {
        *c = None;
    }
    if let Some(h) = hitstopp.as_deref_mut() {
        *h = false;
    }

    // A back reference to a string already matched is matched against that string.
    if d.backno >= 0 && v.pmatch[d.backno as usize].rm_so >= 0 {
        let cp = dfa_backref(v, d, start, min, max, true);
        if cp.is_some() {
            if let Some(c) = coldp {
                *c = Some(start);
            }
        }
        return cp;
    }

    // An NFA that matches every string of a range of lengths.
    if d.cnfa.flags & MATCHALL != 0 {
        let nchr = (min - start) as i64;

        if d.cnfa.maxmatchall != DUPINF && nchr > d.cnfa.maxmatchall as i64 {
            return None;
        }
        if ((max - start) as i64) < d.cnfa.minmatchall as i64 {
            return None;
        }
        if nchr < d.cnfa.minmatchall as i64 {
            min = start + d.cnfa.minmatchall as usize;
        }
        if let Some(c) = coldp {
            *c = Some(start);
        }
        return Some(min);
    }

    let mut css = initialize(v, d, start)?;
    let mut cp = start;

    // The startup.
    let co = startcolor(v, d, cp);
    css = miss(v, d, css, co, cp, start)?;
    d.ssets[css].lastseen = Some(cp);
    let mut ss = Some(css);

    // The scan.
    while cp < realmax {
        let co = cm.getcolor(v.string[cp]);
        let mut s = d.outs(css, co);
        if s == NONE {
            match miss(v, d, css, co, cp + 1, start) {
                Some(x) => s = x as u32,
                None => {
                    ss = None;
                    break;
                }
            }
        }
        cp += 1;
        let s = s as usize;
        d.ssets[s].lastseen = Some(cp);
        css = s;
        ss = Some(s);
        if d.flags(s) & POSTSTATE != 0 && cp >= realmin {
            break;
        }
    }

    let s = ss?;

    if let Some(c) = coldp {
        *c = Some(lastcold(d));
    }

    if d.flags(s) & POSTSTATE != 0 && cp > min {
        cp -= 1;
    } else if cp == v.stop && max == v.stop {
        let co = eoscolor(v, d);
        ss = miss(v, d, css, co, cp, start);
        // The match might have ended at the string's end.
        if ss.is_none_or(|s| d.flags(s) & POSTSTATE == 0) {
            if let Some(h) = hitstopp {
                *h = true;
            }
        }
    }

    match ss {
        Some(s) if d.flags(s) & POSTSTATE != 0 => Some(cp),
        _ => None,
    }
}

/// `matchuntil`: whether the DFA, a search NFA's, matches from the string's start to `probe`. The
/// scan is carried from one call to the next in `lastcss` and `lastcp`, both None before the first
/// call, so that probes that never go back cost one scan in all. False on an error too (`v.err`
/// set).
pub(super) fn matchuntil(
    v: &mut Vars,
    d: &mut Dfa,
    probe: usize,
    lastcss: &mut Option<usize>,
    lastcp: &mut Option<usize>,
) -> bool {
    let cm = d.cm;

    // An NFA that matches every string of a range of lengths; a search NFA's has no most.
    if d.cnfa.flags & MATCHALL != 0 {
        return probe as i64 >= d.cnfa.minmatchall as i64;
    }

    // The startup, or a restart where the probe went back.
    let (mut cp, mut css) = match *lastcp {
        Some(cp) if cp <= probe => match *lastcss {
            Some(css) => (cp, css),
            // No match is possible past `lastcp`.
            None => return false,
        },
        _ => {
            let cp = 0;
            let Some(css) = initialize(v, d, cp) else {
                return false;
            };
            let co = d.cnfa.bos[if v.eflags & REG_NOTBOL != 0 { 0 } else { 1 }];
            let Some(css) = miss(v, d, css, co, cp, 0) else {
                return false;
            };
            d.ssets[css].lastseen = Some(cp);
            (cp, css)
        }
    };
    let mut ss = Some(css);

    // The scan.
    while cp < probe {
        let co = cm.getcolor(v.string[cp]);
        let mut s = d.outs(css, co);
        if s == NONE {
            match miss(v, d, css, co, cp + 1, 0) {
                Some(x) => s = x as u32,
                None => {
                    ss = None;
                    break;
                }
            }
        }
        cp += 1;
        let s = s as usize;
        d.ssets[s].lastseen = Some(cp);
        css = s;
        ss = Some(s);
    }

    *lastcss = ss;
    *lastcp = Some(cp);

    if ss.is_none() {
        return false;
    }

    // One more character, or the string's end, decides the match.
    let ss = if cp < v.stop {
        let co = cm.getcolor(v.string[cp]);
        let s = d.outs(css, co);
        if s == NONE {
            miss(v, d, css, co, cp + 1, 0)
        } else {
            Some(s as usize)
        }
    } else {
        let co = eoscolor(v, d);
        miss(v, d, css, co, cp, 0)
    };

    matches!(ss, Some(s) if d.flags(s) & POSTSTATE != 0)
}

/// `dfa_backref`: the end of the longest run (or, where `shortest`, the shortest) of repetitions of
/// the string a back reference refers to that starts at `start` and ends between `min` and `max`;
/// None where there is none.
fn dfa_backref(
    v: &Vars,
    d: &Dfa,
    start: usize,
    min: usize,
    max: usize,
    shortest: bool,
) -> Option<usize> {
    let n = d.backno as usize;
    let backmin = d.backmin as usize;
    let backmax = d.backmax as i32;

    // The string referred to.
    if v.pmatch[n].rm_so == -1 {
        return None;
    }
    let brstring = v.pmatch[n].rm_so as usize;
    let brlen = (v.pmatch[n].rm_eo - v.pmatch[n].rm_so) as usize;

    // An empty string matches only an empty string, any number of times.
    if brlen == 0 {
        if min == start && d.backmin <= d.backmax {
            return Some(start);
        }
        return None;
    }

    // The bounds as numbers of repetitions.
    let mut minreps = if min <= start {
        0
    } else {
        (min - start - 1) / brlen + 1
    };
    let mut maxreps = (max - start) / brlen;

    if minreps < backmin {
        minreps = backmin;
    }
    if backmax != DUPINF && maxreps > backmax as usize {
        maxreps = backmax as usize;
    }
    if maxreps < minreps {
        return None;
    }

    // No repetitions, where that is allowed and preferred.
    if shortest && minreps == 0 {
        return Some(start);
    }

    let mut p = start;
    let mut numreps = 0;
    while numreps < maxreps {
        if !v.g.compare(&v.string[brstring..], &v.string[p..], brlen) {
            break;
        }
        p += brlen;
        numreps += 1;
        if shortest && numreps >= minreps {
            break;
        }
    }

    if numreps >= minreps {
        Some(p)
    } else {
        None
    }
}

/// `lastcold`: the last place where the scan was in a state set that had made no progress.
fn lastcold(d: &Dfa) -> usize {
    let mut nopr = d.lastnopr.unwrap_or(0);
    for ss in &d.ssets[..d.nssused] {
        if let Some(seen) = ss.lastseen {
            if ss.flags & NOPROGRESS != 0 && nopr < seen {
                nopr = seen;
            }
        }
    }
    nopr
}

/// Whether an array of `n` elements of `size` bytes fits in one of PostgreSQL's allocations.
fn fits(n: usize, size: usize) -> bool {
    n.checked_mul(size).is_some_and(|b| b <= MAX_ALLOC_SIZE)
}

/// `newdfa`: a fresh DFA over a compacted NFA; None on an error (`v.err` set): `REG_ETOOBIG` where
/// its arrays would pass C's bounds on them, `REG_ESPACE` where they cannot be had.
pub(super) fn newdfa<'a>(v: &mut Vars, cnfa: &'a Cnfa, cm: &'a ColorMap) -> Option<Dfa<'a>> {
    if cnfa.nstates == 0 {
        v.seterr(REG_ASSERT);
        return None;
    }
    let nss = cnfa.nstates * 2;
    let wordsper = cnfa.nstates.div_ceil(UBITS);
    let ncolors = cnfa.ncolors;

    if !(nss <= FEWSTATES && ncolors <= FEWCOLORS) {
        let int_max = i32::MAX as usize;
        if wordsper >= int_max / (nss + WORK) || ncolors >= int_max / nss {
            v.seterr(REG_ETOOBIG);
            return None;
        }
        if !fits(nss, SIZEOF_SSET)
            || !fits((nss + WORK) * wordsper, SIZEOF_UNSIGNED)
            || !fits(nss * ncolors, SIZEOF_POINTER)
            || !fits(nss * ncolors, SIZEOF_ARCP)
        {
            v.seterr(REG_ESPACE);
            return None;
        }
    }

    let nssets = if v.eflags & REG_SMALL != 0 { 7 } else { nss };

    let mut ssets = Vec::new();
    let mut statesarea = Vec::new();
    let mut outsarea = Vec::new();
    let mut incarea = Vec::new();
    if ssets.try_reserve_exact(nssets).is_err()
        || statesarea.try_reserve_exact(nssets * wordsper).is_err()
        || outsarea.try_reserve_exact(nssets * ncolors).is_err()
        || incarea.try_reserve_exact(nssets * ncolors).is_err()
    {
        v.seterr(REG_ESPACE);
        return None;
    }

    Some(Dfa {
        nssets,
        nssused: 0,
        nstates: cnfa.nstates,
        ncolors,
        wordsper,
        ssets,
        statesarea,
        work: vec![0; wordsper],
        outsarea,
        incarea,
        cnfa,
        cm,
        lastpost: None,
        lastnopr: None,
        search: 0,
        backno: -1,
        backmin: 0,
        backmax: 0,
    })
}

/// `hash`: a state set's bitvector folded into one word.
fn hash(uv: &[u32]) -> u32 {
    uv.iter().fold(0, |h, &w| h ^ w)
}

/// `initialize`: the starting state set, made where the cache does not hold it, and the cache
/// readied for a scan from `start`.
fn initialize(v: &mut Vars, d: &mut Dfa, start: usize) -> Option<usize> {
    let ss = if d.nssused > 0 && d.ssets[0].flags & STARTER != 0 {
        0
    } else {
        let ss = getvacant(v, d, start, start)?;
        let wp = d.wordsper;
        let states = &mut d.statesarea[ss * wp..(ss + 1) * wp];
        states.fill(0);
        bset(states, d.cnfa.pre);
        d.ssets[ss].hash = hash(states);
        d.ssets[ss].flags = STARTER | LOCKED | NOPROGRESS;
        ss
    };

    for s in &mut d.ssets[..d.nssused] {
        s.lastseen = None;
    }
    d.ssets[ss].lastseen = Some(start);
    d.lastpost = None;
    d.lastnopr = None;
    Some(ss)
}

/// `miss`: the state set the DFA reaches from `css` on a character of color `co`, made and cached
/// where the cache does not hold it. `cp` is the place after the character, where lookaround
/// constraints are tested; `start`, where the attempt began. None where no NFA state is reached,
/// or on an error (`v.err` set).
fn miss(
    v: &mut Vars,
    d: &mut Dfa,
    css: usize,
    co: Color,
    cp: usize,
    start: usize,
) -> Option<usize> {
    let cnfa = d.cnfa;

    // It might not be a miss.
    let hit = d.outs(css, co);
    if hit != NONE {
        return Some(hit as usize);
    }

    // The states the plain arcs reach on the character.
    d.work.fill(0);
    let ispseudocolor = d.cm.is_pseudo(co);
    let mut ispost = false;
    let mut noprogress = true;
    let mut gotstate = false;
    for i in 0..d.nstates {
        if !isbset(d.states(css), i) {
            continue;
        }
        let mut ai = cnfa.states[i];
        loop {
            let ca = cnfa.arcs[ai];
            ai += 1;
            if ca.co == COLORLESS {
                break;
            }
            if ca.co == co || (ca.co == RAINBOW && !ispseudocolor) {
                bset(&mut d.work, ca.to);
                gotstate = true;
                if ca.to == cnfa.post {
                    ispost = true;
                }
                if cnfa.stflags[ca.to] & CNFA_NOPROGRESS == 0 {
                    noprogress = false;
                }
            }
        }
    }
    if !gotstate {
        return None;
    }

    // And the states the lookaround constraints' arcs reach from those, until no more are added.
    let mut dolacons = cnfa.flags & HASLACONS != 0;
    let mut sawlacons = false;
    while dolacons {
        dolacons = false;
        for i in 0..d.nstates {
            if !isbset(&d.work, i) {
                continue;
            }
            let mut ai = cnfa.states[i];
            loop {
                let ca = cnfa.arcs[ai];
                ai += 1;
                if ca.co == COLORLESS {
                    break;
                }
                if (ca.co as i64) < cnfa.ncolors as i64 {
                    continue;
                }
                if isbset(&d.work, ca.to) {
                    continue;
                }
                sawlacons = true;
                if !lacon(v, cnfa, cp, ca.co) {
                    if v.err != 0 {
                        return None;
                    }
                    continue;
                }
                if v.err != 0 {
                    return None;
                }
                bset(&mut d.work, ca.to);
                dolacons = true;
                if ca.to == cnfa.post {
                    ispost = true;
                }
                if cnfa.stflags[ca.to] & CNFA_NOPROGRESS == 0 {
                    noprogress = false;
                }
            }
        }
    }
    let h = hash(&d.work);

    // Whether the cache holds the state set already.
    let wp = d.wordsper;
    let mut found = None;
    for p in 0..d.nssused {
        if d.ssets[p].hash == h && (wp == 1 || d.states(p) == &d.work[..]) {
            found = Some(p);
            break;
        }
    }
    let p = match found {
        Some(p) => p,
        None => {
            let p = getvacant(v, d, cp, start)?;
            let Dfa {
                statesarea, work, ..
            } = d;
            statesarea[p * wp..(p + 1) * wp].copy_from_slice(work);
            d.ssets[p].hash = h;
            d.ssets[p].flags = if ispost { POSTSTATE } else { 0 };
            if noprogress {
                d.ssets[p].flags |= NOPROGRESS;
            }
            p
        }
    };

    // The arc is kept, unless a lookaround constraint decided where it leads: then the next
    // crossing of it comes through here again, to test the constraint where it stands then.
    if !sawlacons {
        let at = css * d.ncolors + co as usize;
        d.outsarea[at] = p as u32;
        d.incarea[at] = d.ssets[p].ins;
        d.ssets[p].ins = Arcp { ss: css as u32, co };
    }
    Some(p)
}

/// `lacon`: whether the lookaround constraint of "color" `co` in the NFA `pcnfa` holds at `cp`.
/// False on an error too (`v.err` set).
fn lacon(v: &mut Vars, pcnfa: &Cnfa, cp: usize, co: Color) -> bool {
    // It recurses, so its depth is bounded.
    if v.too_deep(LACON_DEPTH) {
        v.seterr(REG_ETOOBIG);
        return false;
    }

    let g = v.g;
    let n = (co as i64 - pcnfa.ncolors as i64) as usize;
    let Some(sub) = g.lacons.get(n).filter(|_| n > 0) else {
        v.seterr(REG_ASSERT);
        return false;
    };
    let mut d = match v.ladfas[n].take() {
        Some(d) => d,
        None => match newdfa(v, &sub.cnfa, &g.cmap) {
            Some(d) => Box::new(d),
            None => return false,
        },
    };

    v.depth += LACON_DEPTH;
    let satisfied = if latype_is_ahead(sub.latype) {
        let stop = v.stop;
        let end = shortest(v, &mut d, cp, cp, stop, None, None);
        if latype_is_pos(sub.latype) {
            end.is_some()
        } else {
            end.is_none()
        }
    } else {
        // A lookbehind is tested by one scan carried from probe to probe, restarted only where
        // the probe goes back.
        let mut lastcss = v.lblastcss[n];
        let mut lastcp = v.lblastcp[n];
        let matched = matchuntil(v, &mut d, cp, &mut lastcss, &mut lastcp);
        v.lblastcss[n] = lastcss;
        v.lblastcp[n] = lastcp;
        if latype_is_pos(sub.latype) {
            matched
        } else {
            !matched
        }
    };
    v.depth -= LACON_DEPTH;
    v.ladfas[n] = Some(d);
    satisfied
}

/// `getvacant`: a slot for a new state set, its arcs in and out cleared; the rest is the caller's
/// to fill.
fn getvacant(v: &mut Vars, d: &mut Dfa, cp: usize, start: usize) -> Option<usize> {
    let ss = pickss(v, d, cp, start)?;
    let nc = d.ncolors;

    // Its arcs in, those from itself among them.
    let mut ap = d.ssets[ss].ins;
    while ap.ss != NONE {
        let at = ap.ss as usize * nc + ap.co as usize;
        d.outsarea[at] = NONE;
        ap = d.incarea[at];
        d.incarea[at].ss = NONE;
    }
    d.ssets[ss].ins.ss = NONE;

    // Its arcs out, each taken off the chain of arcs in of the state set it reaches.
    for i in 0..nc {
        let p = d.outsarea[ss * nc + i];
        if p == NONE {
            continue;
        }
        let p = p as usize;
        let ins = d.ssets[p].ins;
        if ins.ss as usize == ss && ins.co as usize == i {
            d.ssets[p].ins = d.incarea[ss * nc + i];
        } else {
            let mut lastap = Arcp::NULL;
            let mut ap = ins;
            while ap.ss != NONE && !(ap.ss as usize == ss && ap.co as usize == i) {
                lastap = ap;
                ap = d.incarea[ap.ss as usize * nc + ap.co as usize];
            }
            if ap.ss == NONE || lastap.ss == NONE {
                v.seterr(REG_ASSERT);
                return None;
            }
            d.incarea[lastap.ss as usize * nc + lastap.co as usize] = d.incarea[ss * nc + i];
        }
        d.outsarea[ss * nc + i] = NONE;
        d.incarea[ss * nc + i].ss = NONE;
    }

    // Where a success state was last seen is remembered, and a no-progress state's.
    let s = d.ssets[ss];
    if s.flags & POSTSTATE != 0
        && s.lastseen != d.lastpost
        && (d.lastpost.is_none() || d.lastpost < s.lastseen)
    {
        d.lastpost = s.lastseen;
    }
    if s.flags & NOPROGRESS != 0
        && s.lastseen != d.lastnopr
        && (d.lastnopr.is_none() || d.lastnopr < s.lastseen)
    {
        d.lastnopr = s.lastseen;
    }

    Some(ss)
}

/// `pickss`: the slot the next state set goes in: a new one while the cache has room, else the
/// first unlocked one, from where the last search stopped, not seen within the last two thirds of
/// the cache's size.
fn pickss(v: &mut Vars, d: &mut Dfa, cp: usize, start: usize) -> Option<usize> {
    // The cache is not full.
    if d.nssused < d.nssets {
        let i = d.nssused;
        d.nssused += 1;
        d.ssets.push(Sset {
            hash: 0,
            flags: 0,
            ins: Arcp::NULL,
            lastseen: None,
        });
        d.statesarea.extend(std::iter::repeat_n(0, d.wordsper));
        d.outsarea.extend(std::iter::repeat_n(NONE, d.ncolors));
        d.incarea.extend(std::iter::repeat_n(Arcp::NULL, d.ncolors));
        return Some(i);
    }

    // The oldest, or one old enough.
    let span = d.nssets * 2 / 3;
    let ancient = if cp - start > span { cp - span } else { start };
    let old =
        |s: &Sset| (s.lastseen.is_none() || s.lastseen < Some(ancient)) && s.flags & LOCKED == 0;
    for ss in (d.search..d.nssets).chain(0..d.search) {
        if old(&d.ssets[ss]) {
            d.search = ss + 1;
            return Some(ss);
        }
    }

    // None is old enough, which cannot happen.
    v.seterr(REG_ASSERT);
    None
}
