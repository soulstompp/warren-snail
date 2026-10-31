//! The NFA (regc_nfa.c): states and arcs, the operations the parser strings them with, and the
//! optimization that leaves only PLAIN and LACON arcs, then the NFA compacted for the executor.
//!
//! The states and arcs of every NFA live in the compiler's arenas, by index; each NFA keeps its
//! chain of live states, its free lists, and the space its batches would take, as regc_nfa.c
//! keeps them.

use std::cmp::Ordering;

use super::color::PSEUDO;
use super::comp::*;
use super::guts::*;

/// A state, by its index in `Vars::st`.
pub(super) type StateId = u32;
/// An arc, by its index in `Vars::ar`.
pub(super) type ArcId = u32;
/// An NFA, by its index in `Vars::nfas`.
pub(super) type NfaId = usize;
/// No state, no arc.
pub(super) const NOSTATE: StateId = u32::MAX;
pub(super) const NOARC: ArcId = u32::MAX;

/// The number of a freed state.
const FREESTATE: i32 = -1;

/// An NFA's flag, never a compacted NFA's: it has CANTMATCH arcs.
pub(super) const HASCANTMATCH: i32 = 0o4;

// The space a batch of states or arcs takes in PostgreSQL's compiler, on x86_64: a state 56
// bytes, an arc 72, a batch's header 16.
const STATE_SIZE: usize = 56;
const ARC_SIZE: usize = 72;
const BATCH_HEADER: usize = 16;
const FIRSTSBSIZE: usize = 32;
const MAXSBSIZE: usize = 1024;
const FIRSTABSIZE: usize = 64;
const MAXABSIZE: usize = 1024;

/// The most space the compiler's states and arcs take before a pattern is too complex
/// (`REG_MAX_COMPILE_SPACE`).
pub(super) const REG_MAX_COMPILE_SPACE: usize = 500_000 * (STATE_SIZE + 4 * ARC_SIZE);

fn statebatchsize(n: usize) -> usize {
    n * STATE_SIZE + BATCH_HEADER
}

fn arcbatchsize(n: usize) -> usize {
    n * ARC_SIZE + BATCH_HEADER
}

// The results of `combine`.
const INCOMPATIBLE: i32 = 1;
const SATISFIED: i32 = 2;
const COMPATIBLE: i32 = 3;
const REPLACEARC: i32 = 4;

/// A state (`struct state`).
#[derive(Clone, Copy, Debug)]
pub(super) struct State {
    pub(super) no: i32,
    pub(super) flag: u8,
    pub(super) nins: i32,
    pub(super) nouts: i32,
    pub(super) ins: ArcId,
    pub(super) outs: ArcId,
    pub(super) tmp: StateId,
    pub(super) next: StateId,
    pub(super) prev: StateId,
}

impl State {
    const NEW: State = State {
        no: FREESTATE,
        flag: 0,
        nins: 0,
        nouts: 0,
        ins: NOARC,
        outs: NOARC,
        tmp: NOSTATE,
        next: NOSTATE,
        prev: NOSTATE,
    };
}

/// An arc (`struct arc`); `typ` 0 where it is free.
#[derive(Clone, Copy, Debug)]
pub(super) struct Arc {
    pub(super) typ: i32,
    pub(super) co: Color,
    pub(super) from: StateId,
    pub(super) to: StateId,
    pub(super) outchain: ArcId,
    pub(super) outchain_rev: ArcId,
    pub(super) inchain: ArcId,
    pub(super) inchain_rev: ArcId,
    pub(super) colorchain: ArcId,
    pub(super) colorchain_rev: ArcId,
}

impl Arc {
    const NEW: Arc = Arc {
        typ: 0,
        co: 0,
        from: NOSTATE,
        to: NOSTATE,
        outchain: NOARC,
        outchain_rev: NOARC,
        inchain: NOARC,
        inchain_rev: NOARC,
        colorchain: NOARC,
        colorchain_rev: NOARC,
    };
}

/// An NFA (`struct nfa`).
#[derive(Clone, Debug)]
pub(super) struct Nfa {
    pub(super) pre: StateId,
    pub(super) init: StateId,
    pub(super) final_: StateId,
    pub(super) post: StateId,
    pub(super) nstates: i32,
    pub(super) states: StateId,
    pub(super) slast: StateId,
    pub(super) freestates: StateId,
    pub(super) freearcs: ArcId,
    /// The last batch's size and how much of it is used, and the bytes of all the batches.
    lastsb: usize,
    lastsbused: usize,
    sbbytes: usize,
    lastab: usize,
    lastabused: usize,
    abbytes: usize,
    pub(super) bos: [Color; 2],
    pub(super) eos: [Color; 2],
    pub(super) flags: i32,
    pub(super) minmatchall: i32,
    pub(super) maxmatchall: i32,
    pub(super) parent: Option<NfaId>,
    /// Where its states and arcs begin in the arenas, for a sub-NFA's.
    st_base: usize,
    ar_base: usize,
}

/// A state, by index, as a place.
macro_rules! st {
    ($v:expr, $s:expr) => {
        $v.st[$s as usize]
    };
}
pub(super) use st;

/// An arc, by index, as a place.
macro_rules! ar {
    ($v:expr, $a:expr) => {
        $v.ar[$a as usize]
    };
}
pub(super) use ar;

/// One level of `checkmatchall_recurse`: the state, the arc it is at, what it has found, and
/// the lengths of its paths to the end.
struct MatchallLevel {
    s: StateId,
    a: ArcId,
    result: bool,
    foundloop: bool,
    haspath: Vec<bool>,
}

/// Whether an arc belongs on its color's chain (`COLORED`).
fn colored(a: &Arc) -> bool {
    a.co >= 0 && (a.typ == PLAIN || a.typ == AHEAD || a.typ == BEHIND)
}

/// Whether a bulk move of arcs sorts and merges rather than goes arc by arc
/// (`BULK_ARC_OP_USE_SORT`).
fn bulk_arc_op_use_sort(nsrcarcs: i32, ndestarcs: i32) -> bool {
    if nsrcarcs < 4 {
        false
    } else {
        nsrcarcs > 32 || ndestarcs > 32
    }
}

/// Whether an arc is a constraint's (`isconstraintarc`).
fn isconstraintarc(a: &Arc) -> bool {
    matches!(a.typ, t if t == b'^' as i32 || t == b'$' as i32 || t == BEHIND || t == AHEAD || t == LACON)
}

impl Vars {
    /// A new NFA, with its pre, init, final and post states and their arcs (`newnfa`).
    pub(super) fn newnfa(&mut self, parent: Option<NfaId>) -> Option<NfaId> {
        let nfa = self.nfas.len();
        self.nfas.push(Nfa {
            pre: NOSTATE,
            init: NOSTATE,
            final_: NOSTATE,
            post: NOSTATE,
            nstates: 0,
            states: NOSTATE,
            slast: NOSTATE,
            freestates: NOSTATE,
            freearcs: NOARC,
            lastsb: 0,
            lastsbused: 0,
            sbbytes: 0,
            lastab: 0,
            lastabused: 0,
            abbytes: 0,
            bos: [COLORLESS; 2],
            eos: [COLORLESS; 2],
            flags: 0,
            minmatchall: -1,
            maxmatchall: -1,
            parent,
            st_base: self.st.len(),
            ar_base: self.ar.len(),
        });

        let post = self.newfstate(nfa, b'@');
        self.nfas[nfa].post = post;
        let pre = self.newfstate(nfa, b'>');
        self.nfas[nfa].pre = pre;
        let init = self.newstate(nfa);
        self.nfas[nfa].init = init;
        let fin = self.newstate(nfa);
        self.nfas[nfa].final_ = fin;
        if self.iserr() {
            self.freenfa(nfa);
            return None;
        }
        self.rainbow(nfa, PLAIN, COLORLESS, pre, init);
        self.newarc(nfa, b'^' as i32, 1, pre, init);
        self.newarc(nfa, b'^' as i32, 0, pre, init);
        self.rainbow(nfa, PLAIN, COLORLESS, fin, post);
        self.newarc(nfa, b'$' as i32, 1, fin, post);
        self.newarc(nfa, b'$' as i32, 0, fin, post);

        if self.iserr() {
            self.freenfa(nfa);
            return None;
        }
        Some(nfa)
    }

    /// An NFA freed, its space given back (`freenfa`).
    pub(super) fn freenfa(&mut self, nfa: NfaId) {
        let n = &self.nfas[nfa];
        self.spaceused -= n.sbbytes + n.abbytes;
        if nfa + 1 == self.nfas.len() && nfa > 0 {
            let (sb, ab) = (n.st_base, n.ar_base);
            self.st.truncate(sb);
            self.ar.truncate(ab);
            self.nfas.pop();
        } else {
            let n = &mut self.nfas[nfa];
            n.sbbytes = 0;
            n.abbytes = 0;
            n.nstates = -1;
        }
    }

    /// A new state, its flag 0: NOSTATE after an error (`newstate`).
    pub(super) fn newstate(&mut self, nfa: NfaId) -> StateId {
        let s: StateId;
        let n = &mut self.nfas[nfa];
        if n.freestates != NOSTATE {
            // the free list first
            s = n.freestates;
            n.freestates = self.st[s as usize].next;
        } else if n.lastsb != 0 && n.lastsbused < n.lastsb {
            // then the last batch
            n.lastsbused += 1;
            s = self.st.len() as StateId;
            self.st.push(State::NEW);
        } else {
            // then a new batch
            if self.spaceused >= REG_MAX_COMPILE_SPACE {
                self.verr(REG_ETOOBIG);
                return NOSTATE;
            }
            let n = &mut self.nfas[nfa];
            let nstates = if n.lastsb != 0 {
                n.lastsb * 2
            } else {
                FIRSTSBSIZE
            }
            .min(MAXSBSIZE);
            self.spaceused += statebatchsize(nstates);
            n.sbbytes += statebatchsize(nstates);
            n.lastsb = nstates;
            n.lastsbused = 1;
            s = self.st.len() as StateId;
            self.st.push(State::NEW);
        }

        let n = &mut self.nfas[nfa];
        let slast = n.slast;
        let no = n.nstates;
        n.nstates += 1;
        if n.states == NOSTATE {
            n.states = s;
        }
        n.slast = s;
        st!(self, s) = State {
            no,
            flag: 0,
            nins: 0,
            nouts: 0,
            ins: NOARC,
            outs: NOARC,
            tmp: NOSTATE,
            next: NOSTATE,
            prev: slast,
        };
        if slast != NOSTATE {
            st!(self, slast).next = s;
        }
        s
    }

    /// A new state with a flag (`newfstate`).
    fn newfstate(&mut self, nfa: NfaId, flag: u8) -> StateId {
        let s = self.newstate(nfa);
        if s != NOSTATE {
            st!(self, s).flag = flag;
        }
        s
    }

    /// A state's arcs freed, then the state (`dropstate`).
    pub(super) fn dropstate(&mut self, nfa: NfaId, s: StateId) {
        loop {
            let a = st!(self, s).ins;
            if a == NOARC {
                break;
            }
            self.freearc(nfa, a);
        }
        loop {
            let a = st!(self, s).outs;
            if a == NOARC {
                break;
            }
            self.freearc(nfa, a);
        }
        self.freestate(nfa, s);
    }

    /// A state with no arcs freed onto the free list (`freestate`).
    pub(super) fn freestate(&mut self, nfa: NfaId, s: StateId) {
        let (next, prev) = (st!(self, s).next, st!(self, s).prev);
        st!(self, s).no = FREESTATE;
        st!(self, s).flag = 0;
        if next != NOSTATE {
            st!(self, next).prev = prev;
        } else {
            self.nfas[nfa].slast = prev;
        }
        if prev != NOSTATE {
            st!(self, prev).next = next;
        } else {
            self.nfas[nfa].states = next;
        }
        st!(self, s).prev = NOSTATE;
        // kept on the free list
        st!(self, s).next = self.nfas[nfa].freestates;
        self.nfas[nfa].freestates = s;
    }

    /// A new arc, unless the same arc is there already (`newarc`).
    pub(super) fn newarc(&mut self, nfa: NfaId, t: i32, co: Color, from: StateId, to: StateId) {
        // a duplicate looked for on the shorter chain
        if st!(self, from).nouts <= st!(self, to).nins {
            let mut a = st!(self, from).outs;
            while a != NOARC {
                let x = &ar!(self, a);
                if x.to == to && x.co == co && x.typ == t {
                    return;
                }
                a = x.outchain;
            }
        } else {
            let mut a = st!(self, to).ins;
            while a != NOARC {
                let x = &ar!(self, a);
                if x.from == from && x.co == co && x.typ == t {
                    return;
                }
                a = x.inchain;
            }
        }
        self.createarc(nfa, t, co, from, to);
    }

    /// A new arc, known not to be there already, at the head of its chains (`createarc`).
    pub(super) fn createarc(&mut self, nfa: NfaId, t: i32, co: Color, from: StateId, to: StateId) {
        let a = self.allocarc(nfa);
        if self.iserr() {
            return;
        }
        let (toins, fromouts) = (st!(self, to).ins, st!(self, from).outs);
        ar!(self, a) = Arc {
            typ: t,
            co,
            from,
            to,
            outchain: fromouts,
            outchain_rev: NOARC,
            inchain: toins,
            inchain_rev: NOARC,
            colorchain: NOARC,
            colorchain_rev: NOARC,
        };
        if toins != NOARC {
            ar!(self, toins).inchain_rev = a;
        }
        st!(self, to).ins = a;
        let fromouts = st!(self, from).outs;
        if fromouts != NOARC {
            ar!(self, fromouts).outchain_rev = a;
        }
        st!(self, from).outs = a;
        st!(self, from).nouts += 1;
        st!(self, to).nins += 1;

        if colored(&ar!(self, a)) && self.nfas[nfa].parent.is_none() {
            self.colorchain(a);
        }
    }

    /// A new arc's slot: NOARC after an error (`allocarc`).
    fn allocarc(&mut self, nfa: NfaId) -> ArcId {
        let n = &mut self.nfas[nfa];
        if n.freearcs != NOARC {
            // the free list first
            let a = n.freearcs;
            n.freearcs = self.ar[a as usize].outchain;
            a
        } else if n.lastab != 0 && n.lastabused < n.lastab {
            // then the last batch
            n.lastabused += 1;
            let a = self.ar.len() as ArcId;
            self.ar.push(Arc::NEW);
            a
        } else {
            // then a new batch
            if self.spaceused >= REG_MAX_COMPILE_SPACE {
                self.verr(REG_ETOOBIG);
                return NOARC;
            }
            let n = &mut self.nfas[nfa];
            let narcs = if n.lastab != 0 {
                n.lastab * 2
            } else {
                FIRSTABSIZE
            }
            .min(MAXABSIZE);
            self.spaceused += arcbatchsize(narcs);
            n.abbytes += arcbatchsize(narcs);
            n.lastab = narcs;
            n.lastabused = 1;
            let a = self.ar.len() as ArcId;
            self.ar.push(Arc::NEW);
            a
        }
    }

    /// An arc taken off its chains and freed onto the free list (`freearc`).
    pub(super) fn freearc(&mut self, nfa: NfaId, victim: ArcId) {
        let v = ar!(self, victim);
        let (from, to) = (v.from, v.to);

        // off its color's chain
        if colored(&v) && self.nfas[nfa].parent.is_none() {
            self.uncolorchain(victim);
        }

        // off its source's chain
        let predecessor = v.outchain_rev;
        if predecessor == NOARC {
            st!(self, from).outs = v.outchain;
        } else {
            ar!(self, predecessor).outchain = v.outchain;
        }
        if v.outchain != NOARC {
            ar!(self, v.outchain).outchain_rev = predecessor;
        }
        st!(self, from).nouts -= 1;

        // off its target's chain
        let predecessor = v.inchain_rev;
        if predecessor == NOARC {
            st!(self, to).ins = v.inchain;
        } else {
            ar!(self, predecessor).inchain = v.inchain;
        }
        if v.inchain != NOARC {
            ar!(self, v.inchain).inchain_rev = predecessor;
        }
        st!(self, to).nins -= 1;

        // onto the free list
        let freearcs = self.nfas[nfa].freearcs;
        let x = &mut ar!(self, victim);
        x.typ = 0;
        x.from = NOSTATE;
        x.to = NOSTATE;
        x.inchain = NOARC;
        x.inchain_rev = NOARC;
        x.outchain = freearcs;
        x.outchain_rev = NOARC;
        self.nfas[nfa].freearcs = victim;
    }

    /// An arc moved to another source, which has no such arc (`changearcsource`).
    fn changearcsource(&mut self, a: ArcId, newfrom: StateId) {
        let oldfrom = ar!(self, a).from;

        // off the old source's chain
        let predecessor = ar!(self, a).outchain_rev;
        let next = ar!(self, a).outchain;
        if predecessor == NOARC {
            st!(self, oldfrom).outs = next;
        } else {
            ar!(self, predecessor).outchain = next;
        }
        if next != NOARC {
            ar!(self, next).outchain_rev = predecessor;
        }
        st!(self, oldfrom).nouts -= 1;

        ar!(self, a).from = newfrom;

        // onto the head of the new source's chain
        let head = st!(self, newfrom).outs;
        ar!(self, a).outchain = head;
        ar!(self, a).outchain_rev = NOARC;
        if head != NOARC {
            ar!(self, head).outchain_rev = a;
        }
        st!(self, newfrom).outs = a;
        st!(self, newfrom).nouts += 1;
    }

    /// An arc moved to another target, which has no such arc (`changearctarget`).
    fn changearctarget(&mut self, a: ArcId, newto: StateId) {
        let oldto = ar!(self, a).to;

        // off the old target's chain
        let predecessor = ar!(self, a).inchain_rev;
        let next = ar!(self, a).inchain;
        if predecessor == NOARC {
            st!(self, oldto).ins = next;
        } else {
            ar!(self, predecessor).inchain = next;
        }
        if next != NOARC {
            ar!(self, next).inchain_rev = predecessor;
        }
        st!(self, oldto).nins -= 1;

        ar!(self, a).to = newto;

        // onto the head of the new target's chain
        let head = st!(self, newto).ins;
        ar!(self, a).inchain = head;
        ar!(self, a).inchain_rev = NOARC;
        if head != NOARC {
            ar!(self, head).inchain_rev = a;
        }
        st!(self, newto).ins = a;
        st!(self, newto).nins += 1;
    }

    /// Whether a state has an arc out that is not EMPTY (`hasnonemptyout`).
    fn hasnonemptyout(&self, s: StateId) -> bool {
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if ar!(self, a).typ != EMPTY {
                return true;
            }
            a = ar!(self, a).outchain;
        }
        false
    }

    /// An arc out of a state of a kind and color, if any (`findarc`).
    pub(super) fn findarc(&self, s: StateId, typ: i32, co: Color) -> ArcId {
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if ar!(self, a).typ == typ && ar!(self, a).co == co {
                return a;
            }
            a = ar!(self, a).outchain;
        }
        NOARC
    }

    /// A new arc like another, between other states (`cparc`).
    pub(super) fn cparc(&mut self, nfa: NfaId, oa: ArcId, from: StateId, to: StateId) {
        let (t, co) = (ar!(self, oa).typ, ar!(self, oa).co);
        self.newarc(nfa, t, co, from, to);
    }

    /// Two arcs in to one state ordered by source, color and kind (`sortins_cmp`).
    fn sortins_cmp(&self, a: ArcId, b: ArcId) -> Ordering {
        let (aa, bb) = (&ar!(self, a), &ar!(self, b));
        st!(self, aa.from)
            .no
            .cmp(&st!(self, bb.from).no)
            .then(aa.co.cmp(&bb.co))
            .then(aa.typ.cmp(&bb.typ))
    }

    /// Two arcs out of one state ordered by target, color and kind (`sortouts_cmp`).
    fn sortouts_cmp(&self, a: ArcId, b: ArcId) -> Ordering {
        let (aa, bb) = (&ar!(self, a), &ar!(self, b));
        st!(self, aa.to)
            .no
            .cmp(&st!(self, bb.to).no)
            .then(aa.co.cmp(&bb.co))
            .then(aa.typ.cmp(&bb.typ))
    }

    /// A state's arcs in sorted by source, color and kind (`sortins`).
    fn sortins(&mut self, s: StateId) {
        let n = st!(self, s).nins as usize;
        if n <= 1 {
            return;
        }
        let mut sortarray = Vec::with_capacity(n);
        let mut a = st!(self, s).ins;
        while a != NOARC {
            sortarray.push(a);
            a = ar!(self, a).inchain;
        }
        sortarray.sort_by(|&x, &y| self.sortins_cmp(x, y));
        st!(self, s).ins = sortarray[0];
        for i in 0..n {
            let a = sortarray[i];
            ar!(self, a).inchain = if i + 1 < n { sortarray[i + 1] } else { NOARC };
            ar!(self, a).inchain_rev = if i > 0 { sortarray[i - 1] } else { NOARC };
        }
    }

    /// A state's arcs out sorted by target, color and kind (`sortouts`).
    fn sortouts(&mut self, s: StateId) {
        let n = st!(self, s).nouts as usize;
        if n <= 1 {
            return;
        }
        let mut sortarray = Vec::with_capacity(n);
        let mut a = st!(self, s).outs;
        while a != NOARC {
            sortarray.push(a);
            a = ar!(self, a).outchain;
        }
        sortarray.sort_by(|&x, &y| self.sortouts_cmp(x, y));
        st!(self, s).outs = sortarray[0];
        for i in 0..n {
            let a = sortarray[i];
            ar!(self, a).outchain = if i + 1 < n { sortarray[i + 1] } else { NOARC };
            ar!(self, a).outchain_rev = if i > 0 { sortarray[i - 1] } else { NOARC };
        }
    }

    /// A state's arcs in moved to another state, without duplicates (`moveins`).
    pub(super) fn moveins(&mut self, nfa: NfaId, old: StateId, new: StateId) {
        if st!(self, new).nins == 0 {
            // no duplicates to fear
            loop {
                let a = st!(self, old).ins;
                if a == NOARC {
                    break;
                }
                let (t, co, from) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).from);
                self.createarc(nfa, t, co, from, new);
                self.freearc(nfa, a);
            }
        } else if !bulk_arc_op_use_sort(st!(self, old).nins, st!(self, new).nins) {
            // a few, one at a time
            loop {
                let a = st!(self, old).ins;
                if a == NOARC {
                    break;
                }
                let from = ar!(self, a).from;
                self.cparc(nfa, a, from, new);
                self.freearc(nfa, a);
            }
        } else {
            // many: sorted and merged
            self.sortins(old);
            self.sortins(new);
            if self.iserr() {
                return;
            }
            let mut oa = st!(self, old).ins;
            let mut na = st!(self, new).ins;
            while oa != NOARC && na != NOARC {
                let a = oa;
                match self.sortins_cmp(oa, na) {
                    Ordering::Less => {
                        // nothing in new matches oa
                        oa = ar!(self, oa).inchain;
                        self.changearctarget(a, new);
                    }
                    Ordering::Equal => {
                        // a match: on in both, the duplicate dropped
                        oa = ar!(self, oa).inchain;
                        na = ar!(self, na).inchain;
                        self.freearc(nfa, a);
                    }
                    Ordering::Greater => {
                        // on in new only
                        na = ar!(self, na).inchain;
                    }
                }
            }
            while oa != NOARC {
                let a = oa;
                oa = ar!(self, oa).inchain;
                self.changearctarget(a, new);
            }
        }
    }

    /// A state's arcs in copied to a new state with none (`copyins`).
    fn copyins(&mut self, nfa: NfaId, old: StateId, new: StateId) {
        let mut a = st!(self, old).ins;
        while a != NOARC {
            let (t, co, from) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).from);
            self.createarc(nfa, t, co, from, new);
            a = ar!(self, a).inchain;
        }
    }

    /// Arcs, possibly repeated, merged into a state's arcs in (`mergeins`).
    fn mergeins(&mut self, nfa: NfaId, s: StateId, arcarray: &mut Vec<ArcId>) {
        if arcarray.is_empty() {
            return;
        }

        // the state's arcs and the new ones sorted
        self.sortins(s);
        if self.iserr() {
            return;
        }
        arcarray.sort_by(|&x, &y| self.sortins_cmp(x, y));

        // the duplicates dropped
        let mut j = 0;
        for i in 1..arcarray.len() {
            if self.sortins_cmp(arcarray[j], arcarray[i]) == Ordering::Less {
                j += 1;
                arcarray[j] = arcarray[i];
            }
        }
        arcarray.truncate(j + 1);

        // merged into s's chain; new arcs go to its front, out of the walk's way
        let mut i = 0;
        let mut na = st!(self, s).ins;
        while i < arcarray.len() && na != NOARC {
            let a = arcarray[i];
            match self.sortins_cmp(a, na) {
                Ordering::Less => {
                    let (t, co, from) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).from);
                    self.createarc(nfa, t, co, from, s);
                    i += 1;
                }
                Ordering::Equal => {
                    i += 1;
                    na = ar!(self, na).inchain;
                }
                Ordering::Greater => {
                    na = ar!(self, na).inchain;
                }
            }
        }
        while i < arcarray.len() {
            let a = arcarray[i];
            let (t, co, from) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).from);
            self.createarc(nfa, t, co, from, s);
            i += 1;
        }
    }

    /// A state's arcs out moved to another state, without duplicates (`moveouts`).
    pub(super) fn moveouts(&mut self, nfa: NfaId, old: StateId, new: StateId) {
        if st!(self, new).nouts == 0 {
            // no duplicates to fear
            loop {
                let a = st!(self, old).outs;
                if a == NOARC {
                    break;
                }
                let (t, co, to) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).to);
                self.createarc(nfa, t, co, new, to);
                self.freearc(nfa, a);
            }
        } else if !bulk_arc_op_use_sort(st!(self, old).nouts, st!(self, new).nouts) {
            // a few, one at a time
            loop {
                let a = st!(self, old).outs;
                if a == NOARC {
                    break;
                }
                let to = ar!(self, a).to;
                self.cparc(nfa, a, new, to);
                self.freearc(nfa, a);
            }
        } else {
            // many: sorted and merged
            self.sortouts(old);
            self.sortouts(new);
            if self.iserr() {
                return;
            }
            let mut oa = st!(self, old).outs;
            let mut na = st!(self, new).outs;
            while oa != NOARC && na != NOARC {
                let a = oa;
                match self.sortouts_cmp(oa, na) {
                    Ordering::Less => {
                        oa = ar!(self, oa).outchain;
                        self.changearcsource(a, new);
                    }
                    Ordering::Equal => {
                        oa = ar!(self, oa).outchain;
                        na = ar!(self, na).outchain;
                        self.freearc(nfa, a);
                    }
                    Ordering::Greater => {
                        na = ar!(self, na).outchain;
                    }
                }
            }
            while oa != NOARC {
                let a = oa;
                oa = ar!(self, oa).outchain;
                self.changearcsource(a, new);
            }
        }
    }

    /// A state's arcs out copied to a new state with none (`copyouts`).
    pub(super) fn copyouts(&mut self, nfa: NfaId, old: StateId, new: StateId) {
        let mut a = st!(self, old).outs;
        while a != NOARC {
            let (t, co, to) = (ar!(self, a).typ, ar!(self, a).co, ar!(self, a).to);
            self.createarc(nfa, t, co, new, to);
            a = ar!(self, a).outchain;
        }
    }

    /// A state's PLAIN arcs out copied between two other states as arcs of another kind, AHEAD
    /// or BEHIND (`cloneouts`).
    pub(super) fn cloneouts(
        &mut self,
        nfa: NfaId,
        old: StateId,
        from: StateId,
        to: StateId,
        typ: i32,
    ) {
        let mut a = st!(self, old).outs;
        while a != NOARC {
            let co = ar!(self, a).co;
            self.newarc(nfa, typ, co, from, to);
            a = ar!(self, a).outchain;
        }
    }

    /// The sub-NFA from `lp` to `rp`, not `rp` itself, deleted (`delsub`).
    pub(super) fn delsub(&mut self, nfa: NfaId, lp: StateId, rp: StateId) {
        // the end marked
        st!(self, rp).tmp = rp;
        self.deltraverse(nfa, lp, lp);
        if self.iserr() {
            return;
        }
        st!(self, rp).tmp = NOSTATE;
        st!(self, lp).tmp = NOSTATE;
    }

    /// One level of a traversal entered: its frame pushed, charged to the stack.
    fn tpush<T>(&mut self, frames: &mut Vec<T>, f: T) {
        frames.push(f);
        self.cstack += TRAVERSE_FRAME;
    }

    /// One level of a traversal left.
    fn tpop<T>(&mut self, frames: &mut Vec<T>) {
        frames.pop();
        self.cstack -= TRAVERSE_FRAME;
    }

    /// A traversal's levels all left at once, after an error.
    fn tunwind<T>(&mut self, frames: &mut Vec<T>) {
        self.cstack -= frames.len() * TRAVERSE_FRAME;
        frames.clear();
    }

    /// A state's arcs out destroyed, and the states only they reached (`deltraverse`), depth first
    /// on a stack of its own: each level a state and the arc it is following.
    fn deltraverse(&mut self, nfa: NfaId, _leftend: StateId, s: StateId) {
        let mut frames: Vec<(StateId, ArcId, StateId)> = Vec::new();
        self.deltraverse_enter(&mut frames, s);
        while let Some(&(s, a, to)) = frames.last() {
            if a == NOARC {
                // the next arc out, or the state done
                let na = st!(self, s).outs;
                if na == NOARC {
                    st!(self, s).tmp = NOSTATE;
                    self.tpop(&mut frames);
                    continue;
                }
                let nto = ar!(self, na).to;
                *frames.last_mut().expect("a level") = (s, na, nto);
                self.deltraverse_enter(&mut frames, nto);
            } else {
                // the arc's target done
                if self.iserr() {
                    self.tunwind(&mut frames);
                    return;
                }
                self.freearc(nfa, a);
                if st!(self, to).nins == 0 && st!(self, to).tmp == NOSTATE {
                    self.freestate(nfa, to);
                }
                frames.last_mut().expect("a level").1 = NOARC;
            }
        }
    }

    /// One state of `deltraverse` entered, where it has arcs out and is not in progress.
    fn deltraverse_enter(&mut self, frames: &mut Vec<(StateId, ArcId, StateId)>, s: StateId) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        if st!(self, s).nouts == 0 {
            return;
        }
        if st!(self, s).tmp != NOSTATE {
            // in progress already
            return;
        }
        st!(self, s).tmp = s;
        self.tpush(frames, (s, NOARC, NOSTATE));
    }

    /// The sub-NFA from `start` to `stop` copied between `from` and `to` (`dupnfa`); `start` may
    /// be in another NFA.
    pub(super) fn dupnfa(
        &mut self,
        nfa: NfaId,
        start: StateId,
        stop: StateId,
        from: StateId,
        to: StateId,
    ) {
        if start == stop {
            self.newarc(nfa, EMPTY, 0, from, to);
            return;
        }
        st!(self, stop).tmp = to;
        self.duptraverse(nfa, start, from);
        // the tmp fields cleared
        st!(self, stop).tmp = NOSTATE;
        self.cleartraverse(nfa, start);
    }

    /// A state's copy made, or `stmp` taken as it, and its arcs out copied (`duptraverse`),
    /// depth first on a stack of its own: each level a state, the arc it is at, and whether the
    /// arc's target is done.
    fn duptraverse(&mut self, nfa: NfaId, s: StateId, stmp: StateId) {
        let mut frames: Vec<(StateId, ArcId, bool)> = Vec::new();
        self.duptraverse_enter(nfa, &mut frames, s, stmp);
        while let Some(&(s, a, done)) = frames.last() {
            if !done {
                if a == NOARC || self.iserr() {
                    self.tpop(&mut frames);
                    continue;
                }
                frames.last_mut().expect("a level").2 = true;
                let to = ar!(self, a).to;
                self.duptraverse_enter(nfa, &mut frames, to, NOSTATE);
            } else {
                if self.iserr() {
                    self.tpop(&mut frames);
                    continue;
                }
                let to = ar!(self, a).to;
                let (from, to) = (st!(self, s).tmp, st!(self, to).tmp);
                self.cparc(nfa, a, from, to);
                *frames.last_mut().expect("a level") = (s, ar!(self, a).outchain, false);
            }
        }
    }

    /// One state of `duptraverse` entered, where it has no copy yet.
    fn duptraverse_enter(
        &mut self,
        nfa: NfaId,
        frames: &mut Vec<(StateId, ArcId, bool)>,
        s: StateId,
        stmp: StateId,
    ) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        if st!(self, s).tmp != NOSTATE {
            // done already
            return;
        }
        let dup = if stmp == NOSTATE {
            self.newstate(nfa)
        } else {
            stmp
        };
        st!(self, s).tmp = dup;
        if dup == NOSTATE {
            return;
        }
        let outs = st!(self, s).outs;
        self.tpush(frames, (s, outs, false));
    }

    /// The constraints of the sub-NFA from `start` to `stop` made EMPTY arcs
    /// (`removeconstraints`).
    pub(super) fn removeconstraints(&mut self, nfa: NfaId, start: StateId, stop: StateId) {
        if start == stop {
            return;
        }
        st!(self, stop).tmp = stop;
        self.removetraverse(nfa, start);
        // the tmp fields cleared
        st!(self, stop).tmp = NOSTATE;
        self.cleartraverse(nfa, start);
    }

    /// A state's constraint arcs out made EMPTY arcs, and on (`removetraverse`), depth first on a
    /// stack of its own: each level a state, the arc it is at, and whether the arc's target is
    /// done.
    fn removetraverse(&mut self, nfa: NfaId, s: StateId) {
        let mut frames: Vec<(StateId, ArcId, bool)> = Vec::new();
        self.removetraverse_enter(&mut frames, s);
        while let Some(&(s, a, done)) = frames.last() {
            if !done {
                if a == NOARC || self.iserr() {
                    self.tpop(&mut frames);
                    continue;
                }
                frames.last_mut().expect("a level").2 = true;
                let to = ar!(self, a).to;
                self.removetraverse_enter(&mut frames, to);
            } else {
                if self.iserr() {
                    self.tpop(&mut frames);
                    continue;
                }
                let to = ar!(self, a).to;
                let oa = ar!(self, a).outchain;
                match ar!(self, a).typ {
                    PLAIN | EMPTY | CANTMATCH => {}
                    t if t == AHEAD
                        || t == BEHIND
                        || t == b'^' as i32
                        || t == b'$' as i32
                        || t == LACON =>
                    {
                        // replaced
                        self.newarc(nfa, EMPTY, 0, s, to);
                        self.freearc(nfa, a);
                    }
                    _ => self.verr(REG_ASSERT),
                }
                *frames.last_mut().expect("a level") = (s, oa, false);
            }
        }
    }

    /// One state of `removetraverse` entered, where it is not done already.
    fn removetraverse_enter(&mut self, frames: &mut Vec<(StateId, ArcId, bool)>, s: StateId) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        if st!(self, s).tmp != NOSTATE {
            return;
        }
        st!(self, s).tmp = s;
        let outs = st!(self, s).outs;
        self.tpush(frames, (s, outs, false));
    }

    /// The tmp fields a traversal left set cleared (`cleartraverse`), depth first on a stack of
    /// its own: each level a state and its next arc out.
    fn cleartraverse(&mut self, _nfa: NfaId, s: StateId) {
        let mut frames: Vec<(StateId, ArcId)> = Vec::new();
        self.cleartraverse_enter(&mut frames, s);
        while let Some(&(_, a)) = frames.last() {
            if a == NOARC {
                self.tpop(&mut frames);
                continue;
            }
            frames.last_mut().expect("a level").1 = ar!(self, a).outchain;
            let to = ar!(self, a).to;
            self.cleartraverse_enter(&mut frames, to);
        }
    }

    /// One state of `cleartraverse` entered, where its tmp is set.
    fn cleartraverse_enter(&mut self, frames: &mut Vec<(StateId, ArcId)>, s: StateId) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        if st!(self, s).tmp == NOSTATE {
            return;
        }
        st!(self, s).tmp = NOSTATE;
        let outs = st!(self, s).outs;
        self.tpush(frames, (s, outs));
    }

    /// Where getting from `s1` to `s2` crosses one PLAIN arc, of one color or a set: the state
    /// whose arcs out are just those, EMPTY arcs aside; else NOSTATE (`single_color_transition`).
    pub(super) fn single_color_transition(&self, s1: StateId, s2: StateId) -> StateId {
        let mut s1 = s1;
        let mut s2 = s2;
        // a leading EMPTY arc ignored
        if st!(self, s1).nouts == 1 && ar!(self, st!(self, s1).outs).typ == EMPTY {
            s1 = ar!(self, st!(self, s1).outs).to;
        }
        // and a trailing one
        if st!(self, s2).nins == 1 && ar!(self, st!(self, s2).ins).typ == EMPTY {
            s2 = ar!(self, st!(self, s2).ins).from;
        }
        // a loop on one state between is refused
        if s1 == s2 {
            return NOSTATE;
        }
        // at least one arc out, all of them PLAIN arcs to s2
        if st!(self, s1).outs == NOARC {
            return NOSTATE;
        }
        let mut a = st!(self, s1).outs;
        while a != NOARC {
            if ar!(self, a).typ != PLAIN || ar!(self, a).to != s2 {
                return NOSTATE;
            }
            a = ar!(self, a).outchain;
        }
        s1
    }

    /// The colors of the string's and the line's starts and ends, false colors of the main NFA's
    /// own or its parent's (`specialcolors`).
    pub(super) fn specialcolors(&mut self, nfa: NfaId) {
        match self.nfas[nfa].parent {
            None => {
                let b0 = self.pseudocolor();
                let b1 = self.pseudocolor();
                let e0 = self.pseudocolor();
                let e1 = self.pseudocolor();
                let n = &mut self.nfas[nfa];
                n.bos = [b0, b1];
                n.eos = [e0, e1];
            }
            Some(p) => {
                let (bos, eos) = (self.nfas[p].bos, self.nfas[p].eos);
                let n = &mut self.nfas[nfa];
                n.bos = bos;
                n.eos = eos;
            }
        }
    }

    /// An NFA made one the executor can run, of PLAIN and LACON arcs only: what its analysis
    /// noted (`optimize`).
    pub(super) fn optimize(&mut self, nfa: NfaId) -> i64 {
        // CANTMATCH arcs dropped, if any
        if (self.nfas[nfa].flags & HASCANTMATCH) != 0 {
            self.removecantmatch(nfa);
            self.nfas[nfa].flags &= !HASCANTMATCH;
        }
        self.cleanup(nfa);
        self.fixempties(nfa);
        self.fixconstraintloops(nfa);
        self.pullback(nfa);
        self.pushfwd(nfa);
        self.cleanup(nfa);
        self.analyze(nfa)
    }

    /// The back constraints pulled back to the start, then made PLAIN arcs of the start's colors
    /// (`pullback`).
    fn pullback(&mut self, nfa: NfaId) {
        // pulled until there are none to pull
        loop {
            let mut progress = false;
            let mut s = self.nfas[nfa].states;
            while s != NOSTATE && !self.iserr() {
                let nexts = st!(self, s).next;
                let mut intermediates = NOSTATE;
                let mut a = st!(self, s).outs;
                while a != NOARC && !self.iserr() {
                    let nexta = ar!(self, a).outchain;
                    let t = ar!(self, a).typ;
                    if (t == b'^' as i32 || t == BEHIND) && self.pull(nfa, a, &mut intermediates) {
                        progress = true;
                    }
                    a = nexta;
                }
                // the intermediate states' tmp fields cleared
                while intermediates != NOSTATE {
                    let ns = st!(self, intermediates).tmp;
                    st!(self, intermediates).tmp = NOSTATE;
                    intermediates = ns;
                }
                // s dropped where it is now useless
                if (st!(self, s).nins == 0 || st!(self, s).nouts == 0) && st!(self, s).flag == 0 {
                    self.dropstate(nfa, s);
                }
                s = nexts;
            }
            if !(progress && !self.iserr()) {
                break;
            }
        }
        if self.iserr() {
            return;
        }

        // the ^ constraints at the start made PLAIN arcs of BOS's or BOL's color
        let pre = self.nfas[nfa].pre;
        let mut a = st!(self, pre).outs;
        while a != NOARC {
            let nexta = ar!(self, a).outchain;
            if ar!(self, a).typ == b'^' as i32 {
                let co = self.nfas[nfa].bos[ar!(self, a).co as usize];
                let (from, to) = (ar!(self, a).from, ar!(self, a).to);
                self.newarc(nfa, PLAIN, co, from, to);
                self.freearc(nfa, a);
            }
            a = nexta;
        }
    }

    /// A back constraint pulled back past its source state: whether it was (`pull`). No state
    /// there before is deleted, nor any other arc out of the source.
    fn pull(&mut self, nfa: NfaId, con: ArcId, intermediates: &mut StateId) -> bool {
        let mut con = con;
        let mut from = ar!(self, con).from;
        let to = ar!(self, con).to;

        if st!(self, from).flag != 0 {
            // not past the start
            return false;
        }
        if st!(self, from).nins == 0 {
            // unreachable
            self.freearc(nfa, con);
            return true;
        }

        // the source copied, where it has other arcs out
        if st!(self, from).nouts > 1 {
            let s = self.newstate(nfa);
            if self.iserr() {
                return false;
            }
            self.copyins(nfa, from, s);
            self.cparc(nfa, con, s, to);
            self.freearc(nfa, con);
            if self.iserr() {
                return false;
            }
            from = s;
            con = st!(self, from).outs;
        }

        // the constraint carried into the source's arcs in
        let mut a = st!(self, from).ins;
        while a != NOARC && !self.iserr() {
            let nexta = ar!(self, a).inchain;
            match self.combine(con, a) {
                INCOMPATIBLE => {
                    // the arc destroyed
                    self.freearc(nfa, a);
                }
                SATISFIED => {}
                COMPATIBLE => {
                    // the two arcs swapped, more or less, through an intermediate state
                    let afrom = ar!(self, a).from;
                    let mut s = *intermediates;
                    while s != NOSTATE {
                        if ar!(self, st!(self, s).ins).from == afrom
                            && ar!(self, st!(self, s).outs).to == to
                        {
                            break;
                        }
                        s = st!(self, s).tmp;
                    }
                    if s == NOSTATE {
                        s = self.newstate(nfa);
                        if self.iserr() {
                            return false;
                        }
                        st!(self, s).tmp = *intermediates;
                        *intermediates = s;
                    }
                    self.cparc(nfa, con, afrom, s);
                    self.cparc(nfa, a, s, to);
                    self.freearc(nfa, a);
                }
                REPLACEARC => {
                    // the arc's color replaced
                    let (t, co, afrom) = (ar!(self, a).typ, ar!(self, con).co, ar!(self, a).from);
                    self.newarc(nfa, t, co, afrom, to);
                    self.freearc(nfa, a);
                }
                _ => {}
            }
            a = nexta;
        }

        // the remaining arcs in take the constraint
        self.moveins(nfa, from, to);
        self.freearc(nfa, con);
        // from is now useless; pullback drops it
        true
    }

    /// The forward constraints pushed forward to the end, then made PLAIN arcs of the end's
    /// colors (`pushfwd`).
    fn pushfwd(&mut self, nfa: NfaId) {
        loop {
            let mut progress = false;
            let mut s = self.nfas[nfa].states;
            while s != NOSTATE && !self.iserr() {
                let nexts = st!(self, s).next;
                let mut intermediates = NOSTATE;
                let mut a = st!(self, s).ins;
                while a != NOARC && !self.iserr() {
                    let nexta = ar!(self, a).inchain;
                    let t = ar!(self, a).typ;
                    if (t == b'$' as i32 || t == AHEAD) && self.push(nfa, a, &mut intermediates) {
                        progress = true;
                    }
                    a = nexta;
                }
                while intermediates != NOSTATE {
                    let ns = st!(self, intermediates).tmp;
                    st!(self, intermediates).tmp = NOSTATE;
                    intermediates = ns;
                }
                if (st!(self, s).nins == 0 || st!(self, s).nouts == 0) && st!(self, s).flag == 0 {
                    self.dropstate(nfa, s);
                }
                s = nexts;
            }
            if !(progress && !self.iserr()) {
                break;
            }
        }
        if self.iserr() {
            return;
        }

        // the $ constraints at the end made PLAIN arcs of EOS's or EOL's color
        let post = self.nfas[nfa].post;
        let mut a = st!(self, post).ins;
        while a != NOARC {
            let nexta = ar!(self, a).inchain;
            if ar!(self, a).typ == b'$' as i32 {
                let co = self.nfas[nfa].eos[ar!(self, a).co as usize];
                let (from, to) = (ar!(self, a).from, ar!(self, a).to);
                self.newarc(nfa, PLAIN, co, from, to);
                self.freearc(nfa, a);
            }
            a = nexta;
        }
    }

    /// A forward constraint pushed forward past its target state: whether it was (`push`). No
    /// state there before is deleted, nor any other arc in to the target.
    fn push(&mut self, nfa: NfaId, con: ArcId, intermediates: &mut StateId) -> bool {
        let mut con = con;
        let from = ar!(self, con).from;
        let mut to = ar!(self, con).to;

        if st!(self, to).flag != 0 {
            // not past the end
            return false;
        }
        if st!(self, to).nouts == 0 {
            // a dead end
            self.freearc(nfa, con);
            return true;
        }

        // the target copied, where it has other arcs in
        if st!(self, to).nins > 1 {
            let s = self.newstate(nfa);
            if self.iserr() {
                return false;
            }
            self.copyouts(nfa, to, s);
            self.cparc(nfa, con, from, s);
            self.freearc(nfa, con);
            if self.iserr() {
                return false;
            }
            to = s;
            con = st!(self, to).ins;
        }

        // the constraint carried into the target's arcs out
        let mut a = st!(self, to).outs;
        while a != NOARC && !self.iserr() {
            let nexta = ar!(self, a).outchain;
            match self.combine(con, a) {
                INCOMPATIBLE => {
                    self.freearc(nfa, a);
                }
                SATISFIED => {}
                COMPATIBLE => {
                    let ato = ar!(self, a).to;
                    let mut s = *intermediates;
                    while s != NOSTATE {
                        if ar!(self, st!(self, s).ins).from == from
                            && ar!(self, st!(self, s).outs).to == ato
                        {
                            break;
                        }
                        s = st!(self, s).tmp;
                    }
                    if s == NOSTATE {
                        s = self.newstate(nfa);
                        if self.iserr() {
                            return false;
                        }
                        st!(self, s).tmp = *intermediates;
                        *intermediates = s;
                    }
                    self.cparc(nfa, con, s, ato);
                    self.cparc(nfa, a, from, s);
                    self.freearc(nfa, a);
                }
                REPLACEARC => {
                    let (t, co, ato) = (ar!(self, a).typ, ar!(self, con).co, ar!(self, a).to);
                    self.newarc(nfa, t, co, from, ato);
                    self.freearc(nfa, a);
                }
                _ => {}
            }
            a = nexta;
        }

        // the remaining arcs out take the constraint
        self.moveouts(nfa, to, from);
        self.freearc(nfa, con);
        true
    }

    /// What a constraint does to an arc it lands on (`combine`).
    fn combine(&self, con: ArcId, a: ArcId) -> i32 {
        let (con, a) = (&ar!(self, con), &ar!(self, a));
        let (ct, at) = (con.typ, a.typ);
        let caret = b'^' as i32;
        let dollar = b'$' as i32;
        let pseudo = |co: Color| (self.cm.cd[co as usize].flags & PSEUDO) != 0;

        // a color constraint meeting a color, or a like constraint
        let colors = || {
            if con.co == a.co {
                return SATISFIED;
            }
            if con.co == RAINBOW {
                // satisfied unless the arc's color is a pseudocolor
                if !pseudo(a.co) {
                    return SATISFIED;
                }
            } else if a.co == RAINBOW {
                // incompatible where it is for a pseudocolor
                if pseudo(con.co) {
                    return INCOMPATIBLE;
                }
                // otherwise the arc is held to the constraint's color
                return REPLACEARC;
            }
            INCOMPATIBLE
        };

        if (ct == caret || ct == dollar) && at == PLAIN {
            // newlines are handled apart
            INCOMPATIBLE
        } else if (ct == AHEAD || ct == BEHIND) && at == PLAIN {
            colors()
        } else if (ct == caret && at == caret) || (ct == dollar && at == dollar) {
            // like constraints: the same, or not
            if con.co == a.co {
                SATISFIED
            } else {
                INCOMPATIBLE
            }
        } else if (ct == AHEAD && at == AHEAD) || (ct == BEHIND && at == BEHIND) {
            colors()
        } else if (ct == caret && at == BEHIND)
            || (ct == BEHIND && at == caret)
            || (ct == dollar && at == AHEAD)
            || (ct == AHEAD && at == dollar)
        {
            // unlike constraints colliding
            INCOMPATIBLE
        } else if (ct == caret && at == dollar)
            || (ct == caret && at == AHEAD)
            || (ct == BEHIND && at == dollar)
            || (ct == BEHIND && at == AHEAD)
            || (ct == dollar && at == caret)
            || (ct == dollar && at == BEHIND)
            || (ct == AHEAD && at == caret)
            || (ct == AHEAD && at == BEHIND)
            || (ct == caret && at == LACON)
            || (ct == BEHIND && at == LACON)
            || (ct == dollar && at == LACON)
            || (ct == AHEAD && at == LACON)
        {
            // constraints passing each other
            COMPATIBLE
        } else {
            INCOMPATIBLE
        }
    }

    /// The EMPTY arcs removed, each chain of them replaced by copies of the arcs into its start
    /// (`fixempties`).
    fn fixempties(&mut self, nfa: NfaId) {
        // states whose one arc out is EMPTY merged into its target
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE && !self.iserr() {
            let nexts = st!(self, s).next;
            if st!(self, s).flag != 0 || st!(self, s).nouts != 1 {
                s = nexts;
                continue;
            }
            let a = st!(self, s).outs;
            if ar!(self, a).typ != EMPTY {
                s = nexts;
                continue;
            }
            let to = ar!(self, a).to;
            if s != to {
                self.moveins(nfa, s, to);
            }
            self.dropstate(nfa, s);
            s = nexts;
        }

        // and states whose one arc in is EMPTY merged into its source
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE && !self.iserr() {
            let nexts = st!(self, s).next;
            if st!(self, s).flag != 0 || st!(self, s).nins != 1 {
                s = nexts;
                continue;
            }
            let a = st!(self, s).ins;
            if ar!(self, a).typ != EMPTY {
                s = nexts;
                continue;
            }
            let from = ar!(self, a).from;
            if s != from {
                self.moveouts(nfa, s, from);
            }
            self.dropstate(nfa, s);
            s = nexts;
        }

        if self.iserr() {
            return;
        }

        // each state's first original arc in, and how many there are in all; arcs added go to
        // the front of the chains, so the original ones are last
        let nstates = self.nfas[nfa].nstates.max(0) as usize;
        let mut inarcsorig = vec![NOARC; nstates];
        let mut totalinarcs = 0usize;
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            inarcsorig[st!(self, s).no as usize] = st!(self, s).ins;
            totalinarcs += st!(self, s).nins as usize;
            s = st!(self, s).next;
        }

        let mut arcarray: Vec<ArcId> = Vec::with_capacity(totalinarcs);

        // each target state
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE && !self.iserr() {
            // a state with no arc out but EMPTY ones is useless after
            if st!(self, s).flag == 0 && !self.hasnonemptyout(s) {
                s = st!(self, s).next;
                continue;
            }

            // the original arcs into the states that reach it by EMPTY arcs
            arcarray.clear();
            let mut s2 = self.emptyreachable(nfa, s, s, &inarcsorig);
            while s2 != s {
                let mut a = inarcsorig[st!(self, s2).no as usize];
                while a != NOARC {
                    if ar!(self, a).typ != EMPTY {
                        arcarray.push(a);
                    }
                    a = ar!(self, a).inchain;
                }
                // the tmp fields reset on the way back
                let nexts = st!(self, s2).tmp;
                st!(self, s2).tmp = NOSTATE;
                s2 = nexts;
            }
            st!(self, s).tmp = NOSTATE;

            let prevnins = st!(self, s).nins;

            // the new ones added
            self.mergeins(nfa, s, &mut arcarray);

            // the state's first original arc in, past the new ones
            let mut nskip = st!(self, s).nins - prevnins;
            let mut a = st!(self, s).ins;
            while nskip > 0 {
                a = ar!(self, a).inchain;
                nskip -= 1;
            }
            inarcsorig[st!(self, s).no as usize] = a;
            s = st!(self, s).next;
        }

        if self.iserr() {
            return;
        }

        // the EMPTY arcs removed
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            let mut a = st!(self, s).outs;
            while a != NOARC {
                let nexta = ar!(self, a).outchain;
                if ar!(self, a).typ == EMPTY {
                    self.freearc(nfa, a);
                }
                a = nexta;
            }
            s = st!(self, s).next;
        }

        // and the states left useless
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            let nexts = st!(self, s).next;
            if (st!(self, s).nins == 0 || st!(self, s).nouts == 0) && st!(self, s).flag == 0 {
                self.dropstate(nfa, s);
            }
            s = nexts;
        }
    }

    /// The states that reach `s` by EMPTY arcs found: the last found, whose tmp leads back
    /// through the others to `s` (`emptyreachable`).
    fn emptyreachable(
        &mut self,
        _nfa: NfaId,
        s: StateId,
        lastfound: StateId,
        inarcsorig: &[ArcId],
    ) -> StateId {
        // depth first on a stack of its own: each level a state and its next original arc in
        let mut frames: Vec<(StateId, ArcId)> = Vec::new();
        let mut lastfound = lastfound;
        self.emptyreachable_enter(&mut frames, s, &mut lastfound, inarcsorig);
        while let Some(&(_, a)) = frames.last() {
            if a == NOARC {
                self.tpop(&mut frames);
                continue;
            }
            frames.last_mut().expect("a level").1 = ar!(self, a).inchain;
            let from = ar!(self, a).from;
            if ar!(self, a).typ == EMPTY && st!(self, from).tmp == NOSTATE {
                self.emptyreachable_enter(&mut frames, from, &mut lastfound, inarcsorig);
            }
        }
        lastfound
    }

    /// One state of `emptyreachable` found: its tmp linked to the last found, and it the last.
    fn emptyreachable_enter(
        &mut self,
        frames: &mut Vec<(StateId, ArcId)>,
        s: StateId,
        lastfound: &mut StateId,
        inarcsorig: &[ArcId],
    ) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        st!(self, s).tmp = *lastfound;
        *lastfound = s;
        let first = inarcsorig[st!(self, s).no as usize];
        self.tpush(frames, (s, first));
    }

    /// Whether a state has a constraint arc out (`hasconstraintout`).
    fn hasconstraintout(&self, s: StateId) -> bool {
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if isconstraintarc(&ar!(self, a)) {
                return true;
            }
            a = ar!(self, a).outchain;
        }
        false
    }

    /// The loops of constraint arcs alone removed (`fixconstraintloops`).
    fn fixconstraintloops(&mut self, nfa: NfaId) {
        // a constraint looping on its state dropped; whether any constraints are left noted
        let mut hasconstraints = false;
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE && !self.iserr() {
            let nexts = st!(self, s).next;
            let mut a = st!(self, s).outs;
            while a != NOARC && !self.iserr() {
                let nexta = ar!(self, a).outchain;
                if isconstraintarc(&ar!(self, a)) {
                    if ar!(self, a).to == s {
                        self.freearc(nfa, a);
                    } else {
                        hasconstraints = true;
                    }
                }
                a = nexta;
            }
            // with no arcs out left, the state is useless
            if st!(self, s).nouts == 0 && st!(self, s).flag == 0 {
                self.dropstate(nfa, s);
            }
            s = nexts;
        }

        if self.iserr() || !hasconstraints {
            return;
        }

        // from each state, a loop of constraints looked for; one found is broken and the search
        // begun again
        'restart: loop {
            let mut s = self.nfas[nfa].states;
            while s != NOSTATE && !self.iserr() {
                if self.findconstraintloop(nfa, s) {
                    continue 'restart;
                }
                s = st!(self, s).next;
            }
            break;
        }

        if self.iserr() {
            return;
        }

        // the states left useless removed, and the tmp fields cleared
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            let nexts = st!(self, s).next;
            st!(self, s).tmp = NOSTATE;
            if (st!(self, s).nins == 0 || st!(self, s).nouts == 0) && st!(self, s).flag == 0 {
                self.dropstate(nfa, s);
            }
            s = nexts;
        }
    }

    /// A loop of constraint arcs reachable from `s` found and broken: whether one was
    /// (`findconstraintloop`). A state known to be in none holds itself in tmp.
    fn findconstraintloop(&mut self, nfa: NfaId, s: StateId) -> bool {
        // depth first on a stack of its own: each level a state and its next arc out
        let mut frames: Vec<(StateId, ArcId)> = Vec::new();
        if let Some(found) = self.findconstraintloop_enter(nfa, &mut frames, s) {
            return found;
        }
        while let Some(&(s, a)) = frames.last() {
            if a == NOARC {
                // no loop leads out of s
                st!(self, s).tmp = s;
                self.tpop(&mut frames);
                continue;
            }
            frames.last_mut().expect("a level").1 = ar!(self, a).outchain;
            if isconstraintarc(&ar!(self, a)) {
                let sto = ar!(self, a).to;
                st!(self, s).tmp = sto;
                if self.findconstraintloop_enter(nfa, &mut frames, sto) == Some(true) {
                    self.tunwind(&mut frames);
                    return true;
                }
            }
        }
        false
    }

    /// One state of `findconstraintloop` entered: what it finds where it is known already, a loop
    /// broken where it is on the path; None where its arcs are to be followed.
    fn findconstraintloop_enter(
        &mut self,
        nfa: NfaId,
        frames: &mut Vec<(StateId, ArcId)>,
        s: StateId,
    ) -> Option<bool> {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            // to leave as soon as can be
            return Some(true);
        }
        let tmp = st!(self, s).tmp;
        if tmp != NOSTATE {
            // known to be in none
            if tmp == s {
                return Some(false);
            }
            // a loop through s
            self.breakconstraintloop(nfa, s);
            return Some(true);
        }
        let outs = st!(self, s).outs;
        self.tpush(frames, (s, outs));
        None
    }

    /// A loop of constraint arcs broken, `sinitial` one of its states and each state's tmp its
    /// successor in the loop (`breakconstraintloop`): the successor of the step chosen cloned,
    /// with the states it reaches by constraints, and the step's arcs led to the clone.
    fn breakconstraintloop(&mut self, nfa: NfaId, sinitial: StateId) {
        // the step to break at: one with a lone constraint arc where there is one
        let mut refarc = NOARC;
        let mut s = sinitial;
        loop {
            let nexts = st!(self, s).tmp;
            if refarc == NOARC {
                let mut narcs = 0;
                let mut a = st!(self, s).outs;
                while a != NOARC {
                    if ar!(self, a).to == nexts && isconstraintarc(&ar!(self, a)) {
                        refarc = a;
                        narcs += 1;
                    }
                    a = ar!(self, a).outchain;
                }
                if narcs > 1 {
                    // several, no good
                    refarc = NOARC;
                }
            }
            s = nexts;
            if s == sinitial {
                break;
            }
        }

        let (shead, stail) = if refarc != NOARC {
            // at the lone arc
            (ar!(self, refarc).from, ar!(self, refarc).to)
        } else {
            // else after sinitial
            (sinitial, st!(self, sinitial).tmp)
        };

        // the tmp fields reset, for clonesuccessorstates to use
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            st!(self, s).tmp = NOSTATE;
            s = st!(self, s).next;
        }

        // the clones
        let mut sclone = self.newstate(nfa);
        if sclone == NOSTATE {
            return;
        }
        let nstates = self.nfas[nfa].nstates as usize;
        self.clonesuccessorstates(nfa, stail, sclone, shead, refarc, None, None, nstates);
        if self.iserr() {
            return;
        }

        // a clone with no arcs out is useless
        if st!(self, sclone).nouts == 0 {
            self.freestate(nfa, sclone);
            sclone = NOSTATE;
        }

        // shead's arcs of the loop led to the clone, or dropped
        let mut a = st!(self, shead).outs;
        while a != NOARC {
            let nexta = ar!(self, a).outchain;
            if ar!(self, a).to == stail && isconstraintarc(&ar!(self, a)) {
                if sclone != NOSTATE {
                    self.cparc(nfa, a, shead, sclone);
                }
                self.freearc(nfa, a);
                if self.iserr() {
                    break;
                }
            }
            a = nexta;
        }
    }

    /// The tree of states a constraint loop's successor reaches by constraints, cloned
    /// (`clonesuccessorstates`): `ssource`'s arcs out copied into `sclone`, new clones made for
    /// the states with constraints out, merged into `sclone` where the constraint is known met.
    /// The donemaps mark, by state number, the states already visited for a clone.
    #[allow(clippy::too_many_arguments)]
    fn clonesuccessorstates(
        &mut self,
        nfa: NfaId,
        ssource: StateId,
        sclone: StateId,
        spredecessor: StateId,
        refarc: ArcId,
        curdonemap: Option<&mut Vec<u8>>,
        outerdonemap: Option<&[u8]>,
        nstates: usize,
    ) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }

        // a donemap for this clone, where it has none
        let outermost = curdonemap.is_none();
        let mut own: Vec<u8>;
        let donemap: &mut Vec<u8> = match curdonemap {
            Some(d) => d,
            None => {
                own = match outerdonemap {
                    // the outer level's: states in progress there, or merged, are off limits
                    Some(o) => o.to_vec(),
                    // at the outermost, only spredecessor is
                    None => {
                        let mut d = vec![0u8; nstates];
                        d[st!(self, spredecessor).no as usize] = 1;
                        d
                    }
                };
                &mut own
            }
        };

        // ssource visited
        donemap[st!(self, ssource).no as usize] = 1;

        self.depth += 1;
        self.cstack += TRAVERSE_FRAME;

        // ssource's arcs out cloned, new clone states made but not yet visited; a child clone's
        // tmp holds the state it is a clone of
        let mut a = st!(self, ssource).outs;
        while a != NOARC && !self.iserr() {
            let sto = ar!(self, a).to;
            if isconstraintarc(&ar!(self, a)) && self.hasconstraintout(sto) {
                // a constraint back is not followed, nor a state merged into this clone
                if donemap[st!(self, sto).no as usize] != 0 {
                    a = ar!(self, a).outchain;
                    continue;
                }

                // a child clone of sto made already
                let mut prevclone = NOSTATE;
                let mut a2 = st!(self, sclone).outs;
                while a2 != NOARC {
                    let to2 = ar!(self, a2).to;
                    if st!(self, to2).tmp == sto {
                        prevclone = to2;
                        break;
                    }
                    a2 = ar!(self, a2).outchain;
                }

                // an arc labeled as refarc, or as any arc crossed to reach sclone, is met
                // already: sto's arcs are merged into sclone
                let (at, aco) = (ar!(self, a).typ, ar!(self, a).co);
                let canmerge = if refarc != NOARC
                    && at == ar!(self, refarc).typ
                    && aco == ar!(self, refarc).co
                {
                    true
                } else {
                    let mut found = false;
                    let mut s = sclone;
                    while st!(self, s).ins != NOARC {
                        let ins = st!(self, s).ins;
                        if st!(self, s).nins == 1
                            && at == ar!(self, ins).typ
                            && aco == ar!(self, ins).co
                        {
                            found = true;
                            break;
                        }
                        s = ar!(self, ins).from;
                    }
                    found
                };

                if canmerge {
                    // the child clone, if any, is not needed
                    if prevclone != NOSTATE {
                        self.dropstate(nfa, prevclone);
                    }
                    // sto's arcs merged into sclone
                    self.clonesuccessorstates(
                        nfa,
                        sto,
                        sclone,
                        spredecessor,
                        refarc,
                        Some(&mut *donemap),
                        outerdonemap,
                        nstates,
                    );
                } else if prevclone != NOSTATE {
                    // one more arc to the child clone
                    self.cparc(nfa, a, sclone, prevclone);
                } else {
                    // a new child clone
                    let stoclone = self.newstate(nfa);
                    if stoclone == NOSTATE {
                        break;
                    }
                    st!(self, stoclone).tmp = sto;
                    self.cparc(nfa, a, sclone, stoclone);
                }
            } else {
                // other arcs copied as they are
                self.cparc(nfa, a, sclone, sto);
            }
            a = ar!(self, a).outchain;
        }

        // at this clone's outer level, its child clones visited, their tmp fields cleared
        if outermost {
            let mut a = st!(self, sclone).outs;
            while a != NOARC && !self.iserr() {
                let stoclone = ar!(self, a).to;
                let sto = st!(self, stoclone).tmp;
                if sto != NOSTATE {
                    st!(self, stoclone).tmp = NOSTATE;
                    self.clonesuccessorstates(
                        nfa,
                        sto,
                        stoclone,
                        spredecessor,
                        refarc,
                        None,
                        Some(&donemap[..]),
                        nstates,
                    );
                }
                a = ar!(self, a).outchain;
            }
        }
        self.cstack -= TRAVERSE_FRAME;
        self.depth -= 1;
    }

    /// The CANTMATCH arcs removed, once parsing is done (`removecantmatch`).
    fn removecantmatch(&mut self, nfa: NfaId) {
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            let mut a = st!(self, s).outs;
            while a != NOARC {
                let nexta = ar!(self, a).outchain;
                if ar!(self, a).typ == CANTMATCH {
                    self.freearc(nfa, a);
                    if self.iserr() {
                        return;
                    }
                }
                a = nexta;
            }
            s = st!(self, s).next;
        }
    }

    /// The states the start cannot reach, or that cannot reach the end, dropped, and the rest
    /// numbered again (`cleanup`).
    fn cleanup(&mut self, nfa: NfaId) {
        if self.iserr() {
            return;
        }
        let (pre, post) = (self.nfas[nfa].pre, self.nfas[nfa].post);

        // reachable from pre marked pre, then those reaching post marked post
        self.markreachable(nfa, pre, NOSTATE, pre);
        self.markcanreach(nfa, post, pre, post);
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE && !self.iserr() {
            let nexts = st!(self, s).next;
            if st!(self, s).tmp != post && st!(self, s).flag == 0 {
                self.dropstate(nfa, s);
            }
            s = nexts;
        }
        self.cleartraverse(nfa, pre);

        // the survivors numbered
        let mut n = 0;
        let mut s = self.nfas[nfa].states;
        while s != NOSTATE {
            st!(self, s).no = n;
            n += 1;
            s = st!(self, s).next;
        }
        self.nfas[nfa].nstates = n;
    }

    /// The states reachable from `s` that hold `okay` marked with `mark` (`markreachable`), depth
    /// first on a stack of its own: each level a state and its next arc out.
    fn markreachable(&mut self, _nfa: NfaId, s: StateId, okay: StateId, mark: StateId) {
        let mut frames: Vec<(StateId, ArcId)> = Vec::new();
        self.mark_enter(&mut frames, s, okay, mark, true);
        while let Some(&(_, a)) = frames.last() {
            if a == NOARC {
                self.tpop(&mut frames);
                continue;
            }
            frames.last_mut().expect("a level").1 = ar!(self, a).outchain;
            let to = ar!(self, a).to;
            self.mark_enter(&mut frames, to, okay, mark, true);
        }
    }

    /// The states that reach `s` and hold `okay` marked with `mark` (`markcanreach`), as
    /// `markreachable` on the arcs in.
    fn markcanreach(&mut self, _nfa: NfaId, s: StateId, okay: StateId, mark: StateId) {
        let mut frames: Vec<(StateId, ArcId)> = Vec::new();
        self.mark_enter(&mut frames, s, okay, mark, false);
        while let Some(&(_, a)) = frames.last() {
            if a == NOARC {
                self.tpop(&mut frames);
                continue;
            }
            frames.last_mut().expect("a level").1 = ar!(self, a).inchain;
            let from = ar!(self, a).from;
            self.mark_enter(&mut frames, from, okay, mark, false);
        }
    }

    /// One state of `markreachable` (`forward`) or `markcanreach` entered, where it holds `okay`.
    fn mark_enter(
        &mut self,
        frames: &mut Vec<(StateId, ArcId)>,
        s: StateId,
        okay: StateId,
        mark: StateId,
        forward: bool,
    ) {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return;
        }
        if st!(self, s).tmp != okay {
            return;
        }
        st!(self, s).tmp = mark;
        let first = if forward {
            st!(self, s).outs
        } else {
            st!(self, s).ins
        };
        self.tpush(frames, (s, first));
    }

    /// What an optimized NFA shows: that it matches nothing, or can match the empty string; and
    /// whether it matches every string of a range of lengths (`analyze`).
    fn analyze(&mut self, nfa: NfaId) -> i64 {
        if self.iserr() {
            return 0;
        }
        let (pre, post) = (self.nfas[nfa].pre, self.nfas[nfa].post);

        // nothing matches
        if st!(self, pre).outs == NOARC {
            return REG_UIMPOSSIBLE;
        }

        // every string of some lengths matches
        self.checkmatchall(nfa);

        // the empty string can match
        let mut a = st!(self, pre).outs;
        while a != NOARC {
            let mut aa = st!(self, ar!(self, a).to).outs;
            while aa != NOARC {
                if ar!(self, aa).to == post {
                    return REG_UEMPTYMATCH;
                }
                aa = ar!(self, aa).outchain;
            }
            a = ar!(self, a).outchain;
        }
        0
    }

    /// Whether the NFA tests a string's length and nothing more: if so its fewest and most
    /// characters set, and MATCHALL (`checkmatchall`).
    fn checkmatchall(&mut self, nfa: NfaId) {
        let n = &self.nfas[nfa];
        // too many states to bother
        if n.nstates > DUPINF * 2 {
            return;
        }
        let (pre, post, bos, eos) = (n.pre, n.post, n.bos, n.eos);

        // only RAINBOW arcs, and pseudocolor arcs next to pre and post
        let mut s = n.states;
        while s != NOSTATE {
            let mut a = st!(self, s).outs;
            while a != NOARC {
                let x = ar!(self, a);
                if x.typ != PLAIN {
                    return;
                }
                if x.co != RAINBOW {
                    if (self.cm.cd[x.co as usize].flags & PSEUDO) != 0 {
                        // a pseudocolor arc, in its place
                        if (s == pre && (x.co == bos[0] || x.co == bos[1]))
                            || (x.to == post && (x.co == eos[0] || x.co == eos[1]))
                        {
                        } else {
                            return;
                        }
                    } else {
                        return;
                    }
                }
                a = x.outchain;
            }
            s = st!(self, s).next;
        }

        // pre's BOS and BOL arcs reach what its RAINBOW arcs reach, and post's EOS and EOL arcs
        // come from where its RAINBOW arcs come
        if !self.check_out_colors_match(pre, RAINBOW, bos[0])
            || !self.check_out_colors_match(pre, RAINBOW, bos[1])
            || !self.check_in_colors_match(post, RAINBOW, eos[0])
            || !self.check_in_colors_match(post, RAINBOW, eos[1])
        {
            return;
        }

        // each state's lengths of paths to the end, by its number
        let nstates = self.nfas[nfa].nstates as usize;
        let mut haspaths: Vec<Option<Vec<bool>>> = vec![None; nstates];

        if self.checkmatchall_recurse(nfa, pre, &mut haspaths) {
            let haspath = haspaths[st!(self, pre).no as usize]
                .as_ref()
                .expect("pre's lengths");
            let top = (DUPINF + 1) as usize;

            // the first run of lengths, and none past it
            let mut minmatch = 0usize;
            while minmatch <= top {
                if haspath[minmatch] {
                    break;
                }
                minmatch += 1;
            }
            let mut maxmatch = minmatch;
            while maxmatch < top {
                if !haspath[maxmatch + 1] {
                    break;
                }
                maxmatch += 1;
            }
            let mut ok = true;
            for &h in &haspath[maxmatch + 1..=top] {
                if h {
                    // lengths not consecutive
                    ok = false;
                    break;
                }
            }

            if ok {
                // a path from pre counts the step from pre to init, one more than the string
                let n = &mut self.nfas[nfa];
                n.minmatchall = minmatch as i32 - 1;
                n.maxmatchall = maxmatch as i32 - 1;
                n.flags |= MATCHALL;
            }
        }
    }

    /// The lengths of the all-RAINBOW paths from `s` to the end, kept by state: whether the
    /// search could be made (`checkmatchall_recurse`). A loop of more than one state fails it.
    fn checkmatchall_recurse(
        &mut self,
        nfa: NfaId,
        s: StateId,
        haspaths: &mut [Option<Vec<bool>>],
    ) -> bool {
        // depth first on a stack of its own; a level's result handed to the level above
        let post = self.nfas[nfa].post;
        let top = (DUPINF + 1) as usize;
        let mut frames: Vec<MatchallLevel> = Vec::new();
        if !self.checkmatchall_enter(&mut frames, s) {
            // too deep is only not MATCHALL
            return false;
        }
        // the result of the level just left, for the level above
        let mut returned: Option<bool> = None;
        loop {
            let Some(f) = frames.last_mut() else {
                return returned.unwrap_or(false);
            };
            let mut finish = false;
            if let Some(r) = returned.take() {
                // the arc's target done
                f.result = r;
                if !r {
                    finish = true;
                } else {
                    let x = ar!(self, f.a);
                    let next = haspaths[st!(self, x.to).no as usize]
                        .as_ref()
                        .expect("a visited state's lengths");
                    // a length exactly DUPINF but none longer, or the other way, cannot be shown
                    if next[DUPINF as usize] != next[top] {
                        f.result = false;
                        finish = true;
                    } else {
                        for i in 0..DUPINF as usize {
                            f.haspath[i + 1] |= next[i];
                        }
                        // past DUPINF stays past it
                        f.haspath[top] |= next[top];
                        f.a = x.outchain;
                    }
                }
            } else if f.a == NOARC {
                finish = true;
            } else {
                let x = ar!(self, f.a);
                let s = f.s;
                if x.co != RAINBOW {
                    // pseudocolor arcs ignored
                    f.a = x.outchain;
                } else if x.to == post {
                    // a path to the end, s none away from it
                    f.result = true;
                    f.haspath[0] = true;
                    f.a = x.outchain;
                } else if x.to == s {
                    // a loop of one, dealt with below
                    f.foundloop = true;
                    f.a = x.outchain;
                } else if st!(self, x.to).tmp != NOSTATE {
                    // busy: a loop of more than one
                    f.result = false;
                    finish = true;
                } else if haspaths[st!(self, x.to).no as usize].is_none() {
                    // the paths through the target, found first
                    if !self.checkmatchall_enter(&mut frames, x.to) {
                        returned = Some(false);
                    }
                    continue;
                } else {
                    // a previous visit found paths to the end
                    returned = Some(true);
                    continue;
                }
            }

            if finish {
                let f = frames.last_mut().expect("a level");
                if f.result && f.foundloop {
                    // a loop of one makes every length past the shortest possible
                    let mut i = 0usize;
                    while i <= DUPINF as usize {
                        if f.haspath[i] {
                            break;
                        }
                        i += 1;
                    }
                    i += 1;
                    while i <= top {
                        f.haspath[i] = true;
                        i += 1;
                    }
                }
                let f = frames.pop().expect("a level");
                self.cstack -= TRAVERSE_FRAME;
                haspaths[st!(self, f.s).no as usize] = Some(f.haspath);
                st!(self, f.s).tmp = NOSTATE;
                returned = Some(f.result);
            }
        }
    }

    /// One state of `checkmatchall_recurse` entered: false where the stack is too deep.
    fn checkmatchall_enter(&mut self, frames: &mut Vec<MatchallLevel>, s: StateId) -> bool {
        if self.stack_too_deep() {
            return false;
        }
        // s being visited
        st!(self, s).tmp = s;
        let a = st!(self, s).outs;
        self.tpush(
            frames,
            MatchallLevel {
                s,
                a,
                result: false,
                foundloop: false,
                haspath: vec![false; DUPINF as usize + 2],
            },
        );
        true
    }

    /// Whether the states `s` reaches by arcs of color `co1` are those it reaches by arcs of
    /// `co2` (`check_out_colors_match`).
    fn check_out_colors_match(&mut self, s: StateId, co1: Color, co2: Color) -> bool {
        let mut result = true;
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if ar!(self, a).co == co1 {
                let to = ar!(self, a).to;
                st!(self, to).tmp = to;
            }
            a = ar!(self, a).outchain;
        }
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if ar!(self, a).co == co2 {
                let to = ar!(self, a).to;
                if st!(self, to).tmp != NOSTATE {
                    st!(self, to).tmp = NOSTATE;
                } else {
                    result = false;
                }
            }
            a = ar!(self, a).outchain;
        }
        let mut a = st!(self, s).outs;
        while a != NOARC {
            if ar!(self, a).co == co1 {
                let to = ar!(self, a).to;
                if st!(self, to).tmp != NOSTATE {
                    result = false;
                    st!(self, to).tmp = NOSTATE;
                }
            }
            a = ar!(self, a).outchain;
        }
        result
    }

    /// Whether the states that reach `s` by arcs of color `co1` are those that reach it by arcs
    /// of `co2` (`check_in_colors_match`).
    fn check_in_colors_match(&mut self, s: StateId, co1: Color, co2: Color) -> bool {
        let mut result = true;
        let mut a = st!(self, s).ins;
        while a != NOARC {
            if ar!(self, a).co == co1 {
                let from = ar!(self, a).from;
                st!(self, from).tmp = from;
            }
            a = ar!(self, a).inchain;
        }
        let mut a = st!(self, s).ins;
        while a != NOARC {
            if ar!(self, a).co == co2 {
                let from = ar!(self, a).from;
                if st!(self, from).tmp != NOSTATE {
                    st!(self, from).tmp = NOSTATE;
                } else {
                    result = false;
                }
            }
            a = ar!(self, a).inchain;
        }
        let mut a = st!(self, s).ins;
        while a != NOARC {
            if ar!(self, a).co == co1 {
                let from = ar!(self, a).from;
                if st!(self, from).tmp != NOSTATE {
                    result = false;
                    st!(self, from).tmp = NOSTATE;
                }
            }
            a = ar!(self, a).inchain;
        }
        result
    }

    /// The NFA compacted for the executor: each state's arcs sorted by color then target, LACON
    /// arcs numbered past the colors (`compact`). None after an error.
    pub(super) fn compact(&mut self, nfa: NfaId) -> Option<Cnfa> {
        let n = &self.nfas[nfa];
        let mut nstates = 0usize;
        let mut narcs = 0usize;
        let mut s = n.states;
        while s != NOSTATE {
            nstates += 1;
            // one more for the end marker
            narcs += st!(self, s).nouts as usize + 1;
            s = st!(self, s).next;
        }

        let mut cnfa = Cnfa {
            nstates,
            ncolors: (self.maxcolor() as i32 + 1) as usize,
            flags: n.flags,
            pre: st!(self, n.pre).no as usize,
            post: st!(self, n.post).no as usize,
            bos: n.bos,
            eos: n.eos,
            stflags: vec![0; nstates],
            states: vec![0; nstates],
            arcs: Vec::with_capacity(narcs),
            minmatchall: n.minmatchall,
            maxmatchall: n.maxmatchall,
        };

        let mut s = n.states;
        while s != NOSTATE {
            let no = st!(self, s).no as usize;
            cnfa.stflags[no] = 0;
            cnfa.states[no] = cnfa.arcs.len();
            let first = cnfa.arcs.len();
            let mut a = st!(self, s).outs;
            while a != NOARC {
                let x = ar!(self, a);
                match x.typ {
                    PLAIN => cnfa.arcs.push(Carc {
                        co: x.co,
                        to: st!(self, x.to).no as usize,
                    }),
                    LACON => {
                        // the color it is given must fit
                        if x.co as i32 > MAX_COLOR as i32 - cnfa.ncolors as i32 {
                            self.verr(REG_ECOLORS);
                            return None;
                        }
                        cnfa.arcs.push(Carc {
                            co: (cnfa.ncolors as i32 + x.co as i32) as Color,
                            to: st!(self, x.to).no as usize,
                        });
                        cnfa.flags |= HASLACONS;
                    }
                    _ => {
                        self.verr(REG_ASSERT);
                        return None;
                    }
                }
                a = x.outchain;
            }
            // carcsort
            cnfa.arcs[first..].sort_by(|x, y| x.co.cmp(&y.co).then(x.to.cmp(&y.to)));
            cnfa.arcs.push(Carc {
                co: COLORLESS,
                to: 0,
            });
            s = st!(self, s).next;
        }

        // the states that make no progress marked
        let pre = self.nfas[nfa].pre;
        let mut a = st!(self, pre).outs;
        while a != NOARC {
            cnfa.stflags[st!(self, ar!(self, a).to).no as usize] = CNFA_NOPROGRESS;
            a = ar!(self, a).outchain;
        }
        cnfa.stflags[st!(self, pre).no as usize] = CNFA_NOPROGRESS;
        Some(cnfa)
    }
}
