//! The compiler (regcomp.c): a pattern parsed into an NFA and a tree of subexpressions, each node's
//! NFA and each lookaround constraint's compacted, then the whole NFA made into the one a search
//! starts with.

use super::color::{Cmap, COLMARK, FREECOL, PSEUDO};
use super::cvec::Cvec;
use super::guts::*;
use super::locale::Ctype;
use super::nfa::{ar, st, Arc, Nfa, NfaId, State, StateId, NOARC, NOSTATE};

/// A test flag of `eclass`: `[[=x=]]` stands for x and y (`REG_FAKE`).
pub(super) const REG_FAKE: i32 = 0o10000;

// The kinds of token, some also the kinds of arc.
pub(super) const EMPTY: i32 = b'n' as i32;
pub(super) const EOS: i32 = b'e' as i32;
pub(super) const PLAIN: i32 = b'p' as i32;
pub(super) const DIGIT: i32 = b'd' as i32;
pub(super) const BACKREF: i32 = b'b' as i32;
pub(super) const COLLEL: i32 = b'I' as i32;
pub(super) const ECLASS: i32 = b'E' as i32;
pub(super) const CCLASS: i32 = b'C' as i32;
pub(super) const END: i32 = b'X' as i32;
pub(super) const CCLASSS: i32 = b's' as i32;
pub(super) const CCLASSC: i32 = b'c' as i32;
pub(super) const RANGE: i32 = b'R' as i32;
pub(super) const LACON: i32 = b'L' as i32;
pub(super) const AHEAD: i32 = b'a' as i32;
pub(super) const BEHIND: i32 = b'r' as i32;
pub(super) const WBDRY: i32 = b'w' as i32;
pub(super) const NWBDRY: i32 = b'W' as i32;
pub(super) const CANTMATCH: i32 = b'x' as i32;
pub(super) const SBEGIN: i32 = b'A' as i32;
pub(super) const SEND: i32 = b'Z' as i32;

/// A node of the tree, by its index in `Vars::srs`.
pub(super) type SrId = u32;
/// No node.
pub(super) const NOSR: SrId = u32::MAX;

const UPPROP: i32 = (MIXED | CAP | BACKR) as i32;

/// The flags a node passes up to its parent (`UP`).
fn up(f: i32) -> i32 {
    (f & UPPROP) | ((f << 2) & (f << 1) & MIXED as i32)
}

/// Whether a node's flags make it need the tree's backtracking (`MESSY`).
fn messy(f: i32) -> bool {
    f & (MIXED | CAP | BACKR) as i32 != 0
}

/// A node's preference for longer or shorter matches (`PREF`).
fn pref(f: i32) -> i32 {
    f & (LONGER | SHORTER) as i32
}

/// The first of two preferences that is set (`PREF2`).
fn pref2(f1: i32, f2: i32) -> i32 {
    if pref(f1) != 0 {
        pref(f1)
    } else {
        pref(f2)
    }
}

/// The flags of two nodes joined (`COMBINE`).
fn combine_flags(f1: i32, f2: i32) -> i32 {
    up(f1 | f2) | pref2(f1, f2)
}

/// How deep the parse's recursion goes before the pattern is refused as too complex: the levels of
/// parse, parsebranch and parseqatom, and of the tree's nodes and clones beneath them.
pub(super) const MAX_DEPTH: usize = 4_000;

/// The stack, in bytes, one level of a traversal of the NFA takes in PostgreSQL's compiler.
pub(super) const TRAVERSE_FRAME: usize = 48;

/// The stack, in bytes, PostgreSQL lets the traversals of the NFA take before the pattern is too
/// complex.
pub(super) const C_STACK_BUDGET: usize = 2_097_152;

/// A node of the tree of subexpressions while it is built (`struct subre`).
#[derive(Clone, Debug)]
pub(super) struct Sr {
    pub(super) op: u8,
    pub(super) flags: u8,
    pub(super) latype: u8,
    pub(super) id: i32,
    pub(super) capno: i32,
    pub(super) backno: i32,
    pub(super) min: i16,
    pub(super) max: i16,
    pub(super) child: SrId,
    pub(super) sibling: SrId,
    pub(super) begin: StateId,
    pub(super) end: StateId,
    pub(super) cnfa: Cnfa,
}

impl Default for Sr {
    fn default() -> Sr {
        Sr {
            op: 0,
            flags: 0,
            latype: 0,
            id: 0,
            capno: 0,
            backno: 0,
            min: 0,
            max: 0,
            child: NOSR,
            sibling: NOSR,
            begin: NOSTATE,
            end: NOSTATE,
            cnfa: Cnfa::default(),
        }
    }
}

/// A lookaround constraint while it is built: the sub-NFA it matches, its kind, its compacted NFA.
#[derive(Clone, Debug)]
pub(super) struct Lacon {
    pub(super) begin: StateId,
    pub(super) end: StateId,
    pub(super) latype: u8,
    pub(super) cnfa: Cnfa,
}

/// Everything the compiler works with (`struct vars`), with the states and arcs of every NFA it
/// holds, by index.
pub(super) struct Vars {
    pub(super) pattern: Vec<Chr>,
    /// Where the lexer reads, and the pattern's end.
    pub(super) now: usize,
    pub(super) stop: usize,
    pub(super) err: i32,
    pub(super) cflags: i32,
    pub(super) lasttype: i32,
    pub(super) nexttype: i32,
    pub(super) nextvalue: Chr,
    pub(super) lexcon: i32,
    pub(super) nsubexp: i32,
    /// The capturing subexpressions by number; `nsubs` is its length.
    pub(super) subs: Vec<SrId>,
    pub(super) nsubs: usize,
    /// The main NFA, in `nfas`.
    pub(super) nfa: NfaId,
    pub(super) cm: Cmap,
    pub(super) nlcolor: Color,
    pub(super) wordchrs: StateId,
    pub(super) tree: SrId,
    pub(super) treechain: SrId,
    pub(super) treefree: SrId,
    pub(super) ntree: i32,
    /// The room of the transient character vector (`cv`).
    pub(super) cv_chrspace: usize,
    pub(super) cv_rangespace: usize,
    pub(super) lacons: Vec<Lacon>,
    pub(super) nlacons: i32,
    pub(super) spaceused: usize,
    /// What the compiler notes of the pattern (`re_info`).
    pub(super) info: i64,
    pub(super) ctype: Ctype,
    /// The classes' members, as `regc_ctype_get_cache` keeps them.
    pub(super) class_cache: [Option<Cvec>; NUM_CCLASSES],
    /// How deep the parse's recursion is, and the stack the traversals of the NFA take.
    pub(super) depth: usize,
    pub(super) cstack: usize,
    /// The nodes of the tree, the NFAs, and their states and arcs.
    pub(super) srs: Vec<Sr>,
    pub(super) nfas: Vec<Nfa>,
    pub(super) st: Vec<State>,
    pub(super) ar: Vec<Arc>,
}

/// A node of the tree, by index, as a place.
macro_rules! sr {
    ($v:expr, $t:expr) => {
        $v.srs[$t as usize]
    };
}

impl Vars {
    /// Records an error, the first one recorded staying, and ends the tokens (`VERR`, `ERR`).
    pub(super) fn verr(&mut self, e: i32) {
        self.nexttype = EOS;
        if self.err == 0 {
            self.err = e;
        }
    }

    /// Whether an error was recorded (`ISERR`).
    pub(super) fn iserr(&self) -> bool {
        self.err != 0
    }

    /// Notes a condition of the pattern (`NOTE`).
    pub(super) fn note(&mut self, b: i64) {
        self.info |= b;
    }

    /// Whether the next token is of a kind (`SEE`).
    pub(super) fn see(&self, t: i32) -> bool {
        self.nexttype == t
    }

    /// The next token taken where it is of a kind (`EAT`).
    pub(super) fn eat(&mut self, t: i32) -> bool {
        self.see(t) && self.next() != 0
    }

    /// Whether the recursion has gone as deep as the compiler allows (`rstacktoodeep`).
    pub(super) fn stack_too_deep(&self) -> bool {
        self.depth >= MAX_DEPTH || self.cstack + TRAVERSE_FRAME > C_STACK_BUDGET
    }

    /// An empty arc between two states of the main NFA (`EMPTYARC`).
    fn emptyarc(&mut self, x: StateId, y: StateId) {
        let nfa = self.nfa;
        self.newarc(nfa, EMPTY, 0, x, y);
    }
}

/// A pattern compiled under its flags and classes (`pg_regcomp`): the compiled expression, or the
/// first error its compilation met.
pub(super) fn compile(pattern: &[Chr], cflags: i32, ctype: Ctype) -> Result<Guts, RegError> {
    let flags = cflags;
    if (flags & REG_QUOTE) != 0 && (flags & (REG_ADVANCED | REG_EXPANDED | REG_NEWLINE)) != 0 {
        return Err(RegError(REG_INVARG));
    }
    if (flags & REG_EXTENDED) == 0 && (flags & REG_ADVF) != 0 {
        return Err(RegError(REG_INVARG));
    }

    let mut v = Vars {
        pattern: pattern.to_vec(),
        now: 0,
        stop: pattern.len(),
        err: 0,
        cflags: flags,
        lasttype: 0,
        nexttype: 0,
        nextvalue: 0,
        lexcon: 0,
        nsubexp: 0,
        subs: vec![NOSR; 10],
        nsubs: 10,
        nfa: 0,
        cm: Cmap::initcm(),
        nlcolor: COLORLESS,
        wordchrs: NOSTATE,
        tree: NOSR,
        treechain: NOSR,
        treefree: NOSR,
        ntree: 0,
        cv_chrspace: 100,
        cv_rangespace: 20,
        lacons: Vec::new(),
        nlacons: 0,
        spaceused: 0,
        info: 0,
        ctype,
        class_cache: Default::default(),
        depth: 0,
        cstack: 0,
        srs: Vec::new(),
        nfas: Vec::new(),
        st: Vec::new(),
        ar: Vec::new(),
    };
    macro_rules! cnoerr {
        () => {
            if v.iserr() {
                return Err(RegError(v.err));
            }
        };
    }

    match v.newnfa(None) {
        Some(n) => v.nfa = n,
        None => return Err(RegError(v.err)),
    }

    // parsing
    v.lexstart();
    if (v.cflags & REG_NLSTOP) != 0 || (v.cflags & REG_NLANCH) != 0 {
        // newline gets a color of its own
        v.nlcolor = v.subcolor(super::lex::newline());
        let nfa = v.nfa;
        v.okcolors(nfa);
    }
    cnoerr!();
    let (init, fin) = (v.nfas[v.nfa].init, v.nfas[v.nfa].final_);
    v.tree = v.parse(EOS, PLAIN, init, fin);
    cnoerr!();

    // the NFA's special colors, and the tree made final
    let nfa = v.nfa;
    v.specialcolors(nfa);
    cnoerr!();
    if (v.cflags & REG_NOSUB) != 0 {
        v.removecaptures(v.tree);
    }
    v.ntree = v.numst(v.tree, 1);
    v.markst(v.tree);
    v.cleanst();

    // the compacted NFAs of the tree and of the lookaround constraints
    let r = v.nfatree(v.tree);
    v.info |= r;
    cnoerr!();
    for i in 1..v.nlacons.max(0) as usize {
        let (begin, end, latype) = (v.lacons[i].begin, v.lacons[i].end, v.lacons[i].latype);
        // a lookbehind matches as a search, .* before it
        let (_, cnfa) = v.nfanode(begin, end, !latype_is_ahead(latype));
        if let Some(cnfa) = cnfa {
            v.lacons[i].cnfa = cnfa;
        }
    }
    cnoerr!();
    if (sr!(v, v.tree).flags & SHORTER) != 0 {
        v.note(REG_USHORTEST);
    }

    // the main NFA, made into the search's
    let nfa = v.nfa;
    v.optimize(nfa);
    cnoerr!();
    v.makesearch(nfa);
    cnoerr!();
    let search = v.compact(nfa);
    cnoerr!();
    let search = search.unwrap_or_default();

    Ok(v.package(search))
}

impl Vars {
    /// The compiled expression, from the compiler's work.
    fn package(&mut self, search: Cnfa) -> Guts {
        let ntree = self.ntree as usize;
        let mut subres = vec![Subre::default(); ntree];
        let mut todo = vec![self.tree];
        while let Some(t) = todo.pop() {
            let child = sr!(self, t).child;
            let sibling = sr!(self, t).sibling;
            let child_id = (child != NOSR).then(|| sr!(self, child).id as usize);
            let sibling_id = (sibling != NOSR).then(|| sr!(self, sibling).id as usize);
            let node = &mut self.srs[t as usize];
            let id = node.id as usize;
            subres[id] = Subre {
                op: node.op,
                flags: node.flags,
                latype: node.latype,
                id,
                capno: node.capno as usize,
                backno: node.backno as usize,
                min: node.min,
                max: node.max,
                child: child_id,
                sibling: sibling_id,
                cnfa: std::mem::take(&mut node.cnfa),
            };
            if child != NOSR {
                todo.push(child);
            }
            if sibling != NOSR {
                todo.push(sibling);
            }
        }
        let lacons = self
            .lacons
            .iter_mut()
            .enumerate()
            .map(|(i, la)| Subre {
                latype: la.latype,
                id: i,
                cnfa: std::mem::take(&mut la.cnfa),
                ..Subre::default()
            })
            .collect();
        let cmap = self.cm.runtime(self.ctype);
        Guts {
            cflags: self.cflags,
            info: self.info,
            nsub: self.nsubexp as usize,
            subres,
            ntree,
            search,
            cmap,
            lacons,
            ctype: self.ctype,
        }
    }

    /// The vector of capturing subexpressions made long enough for one more (`moresubs`).
    fn moresubs(&mut self, wanted: i32) {
        let n = wanted as usize * 3 / 2 + 1;
        self.subs.resize(n, NOSR);
        self.nsubs = n;
    }

    /// An NFA made into the one a search starts with, `.*?` implicitly before it, its first states
    /// split where progress could return to them (`makesearch`). The NFA is optimized already.
    pub(super) fn makesearch(&mut self, nfa: NfaId) {
        let pre = self.nfas[nfa].pre;

        // no loops are needed if it's anchored
        let mut a = st!(self, pre).outs;
        while a != NOARC {
            let co = ar!(self, a).co;
            if co != self.nfas[nfa].bos[0] && co != self.nfas[nfa].bos[1] {
                break;
            }
            a = ar!(self, a).outchain;
        }
        if a != NOARC {
            // the implicit .* in front
            self.rainbow(nfa, PLAIN, COLORLESS, pre, pre);
            // and ^* and \A* too
            let (b0, b1) = (self.nfas[nfa].bos[0], self.nfas[nfa].bos[1]);
            self.newarc(nfa, PLAIN, b0, pre, pre);
            self.newarc(nfa, PLAIN, b1, pre, pre);
            // still MATCHALL where it was, with no longest match
            if (self.nfas[nfa].flags & MATCHALL) != 0 {
                self.nfas[nfa].maxmatchall = DUPINF;
            }
        }

        // the states reachable from pre and from elsewhere
        let mut slist = NOSTATE;
        let mut a = st!(self, pre).outs;
        while a != NOARC {
            let s = ar!(self, a).to;
            let mut b = st!(self, s).ins;
            while b != NOARC {
                if ar!(self, b).from != pre {
                    break;
                }
                b = ar!(self, b).inchain;
            }
            // the first in the list holds itself in tmp
            if b != NOARC && st!(self, s).tmp == NOSTATE {
                st!(self, s).tmp = if slist != NOSTATE { slist } else { s };
                slist = s;
            }
            a = ar!(self, a).outchain;
        }

        // the splits
        let mut s = slist;
        while s != NOSTATE {
            let s2 = self.newstate(nfa);
            if self.iserr() {
                return;
            }
            self.copyouts(nfa, s, s2);
            if self.iserr() {
                return;
            }
            let mut a = st!(self, s).ins;
            while a != NOARC {
                let b = ar!(self, a).inchain;
                let from = ar!(self, a).from;
                if from != pre {
                    self.cparc(nfa, a, from, s2);
                    self.freearc(nfa, a);
                }
                a = b;
            }
            let tmp = st!(self, s).tmp;
            let next = if tmp != s { tmp } else { NOSTATE };
            st!(self, s).tmp = NOSTATE;
            s = next;
        }
    }

    /// The branches between `init` and `fin`, tied by `|`, up to `stopper` (EOS or `)`), as one
    /// node, an alternation where there is more than one (`parse`).
    pub(super) fn parse(&mut self, stopper: i32, typ: i32, init: StateId, fin: StateId) -> SrId {
        self.depth += 1;
        let r = self.parse_(stopper, typ, init, fin);
        self.depth -= 1;
        r
    }

    fn parse_(&mut self, stopper: i32, typ: i32, init: StateId, fin: StateId) -> SrId {
        let mut branches = self.subre(b'|', LONGER as i32, init, fin);
        if self.iserr() {
            return NOSR;
        }
        let mut lastbranch = NOSR;
        loop {
            // a branch, with its scaffolding
            let nfa = self.nfa;
            let left = self.newstate(nfa);
            let right = self.newstate(nfa);
            if self.iserr() {
                return NOSR;
            }
            self.emptyarc(init, left);
            self.emptyarc(right, fin);
            if self.iserr() {
                return NOSR;
            }
            let branch = self.parsebranch(stopper, typ, left, right, false);
            if self.iserr() {
                return NOSR;
            }
            if lastbranch != NOSR {
                sr!(self, lastbranch).sibling = branch;
            } else {
                sr!(self, branches).child = branch;
            }
            let f = up(sr!(self, branches).flags as i32 | sr!(self, branch).flags as i32);
            sr!(self, branches).flags |= f as u8;
            lastbranch = branch;
            if !self.eat(b'|' as i32) {
                break;
            }
        }

        if !self.see(stopper) {
            self.verr(REG_EPAREN);
        }

        // the simple cases
        if lastbranch == sr!(self, branches).child {
            // only one branch
            self.freesrnode(branches);
            branches = lastbranch;
        } else if !messy(sr!(self, branches).flags as i32) {
            // nothing of interest inside
            let child = sr!(self, branches).child;
            self.freesubreandsiblings(child);
            sr!(self, branches).child = NOSR;
            sr!(self, branches).op = b'=';
        }
        branches
    }

    /// One branch, its atoms concatenated, between `left` and `right`; `partial` where it is the
    /// rest of a branch begun before (`parsebranch`).
    fn parsebranch(
        &mut self,
        stopper: i32,
        typ: i32,
        left: StateId,
        right: StateId,
        partial: bool,
    ) -> SrId {
        self.depth += 1;
        let r = self.parsebranch_(stopper, typ, left, right, partial);
        self.depth -= 1;
        r
    }

    fn parsebranch_(
        &mut self,
        stopper: i32,
        typ: i32,
        left: StateId,
        right: StateId,
        partial: bool,
    ) -> SrId {
        let mut lp = left;
        let mut seencontent = false;
        let mut t = self.subre(b'=', 0, left, right);
        if self.iserr() {
            return NOSR;
        }
        while !self.see(b'|' as i32) && !self.see(stopper) && !self.see(EOS) {
            if seencontent {
                // the implicit concatenation
                let nfa = self.nfa;
                lp = self.newstate(nfa);
                if self.iserr() {
                    return NOSR;
                }
                self.moveins(nfa, right, lp);
            }
            seencontent = true;

            // the recursion in parseqatom may take the rest of the branch
            t = self.parseqatom(stopper, typ, lp, right, t);
            if self.iserr() {
                return NOSR;
            }
        }

        if !seencontent {
            // an empty branch
            if !partial {
                self.note(REG_UUNSPEC);
            }
            self.emptyarc(left, right);
        }
        t
    }

    /// One quantified atom or constraint between `lp` and `rp`, and where the atom needs the tree,
    /// the rest of the branch with it; `top` the node of the branch so far (`parseqatom`).
    fn parseqatom(&mut self, stopper: i32, typ: i32, lp: StateId, rp: StateId, top: SrId) -> SrId {
        self.depth += 1;
        let r = self.parseqatom_(stopper, typ, lp, rp, top);
        self.depth -= 1;
        r
    }

    fn parseqatom_(
        &mut self,
        stopper: i32,
        typ: i32,
        lp: StateId,
        rp: StateId,
        mut top: SrId,
    ) -> SrId {
        let nfa = self.nfa;
        macro_rules! arcv {
            ($t:expr, $val:expr) => {
                self.newarc(nfa, $t, $val, lp, rp)
            };
        }
        macro_rules! noerrn {
            () => {
                if self.iserr() {
                    return NOSR;
                }
            };
        }

        let mut atom = NOSR;
        let mut subno: i32 = 0;

        // an atom or a constraint...
        let mut atomtype = self.nexttype;
        match atomtype {
            // first the constraints, which return
            t if t == b'^' as i32 => {
                arcv!(b'^' as i32, 1);
                if (self.cflags & REG_NLANCH) != 0 {
                    arcv!(BEHIND, self.nlcolor);
                }
                self.next();
                return top;
            }
            t if t == b'$' as i32 => {
                arcv!(b'$' as i32, 1);
                if (self.cflags & REG_NLANCH) != 0 {
                    arcv!(AHEAD, self.nlcolor);
                }
                self.next();
                return top;
            }
            SBEGIN => {
                arcv!(b'^' as i32, 1); // BOL
                arcv!(b'^' as i32, 0); // or BOS
                self.next();
                return top;
            }
            SEND => {
                arcv!(b'$' as i32, 1); // EOL
                arcv!(b'$' as i32, 0); // or EOS
                self.next();
                return top;
            }
            t if t == b'<' as i32 => {
                self.wordchrs();
                let s = self.newstate(nfa);
                noerrn!();
                self.nonword(BEHIND, lp, s);
                self.word(AHEAD, s, rp);
                self.next();
                return top;
            }
            t if t == b'>' as i32 => {
                self.wordchrs();
                let s = self.newstate(nfa);
                noerrn!();
                self.word(BEHIND, lp, s);
                self.nonword(AHEAD, s, rp);
                self.next();
                return top;
            }
            WBDRY => {
                self.wordchrs();
                let s = self.newstate(nfa);
                noerrn!();
                self.nonword(BEHIND, lp, s);
                self.word(AHEAD, s, rp);
                let s = self.newstate(nfa);
                noerrn!();
                self.word(BEHIND, lp, s);
                self.nonword(AHEAD, s, rp);
                self.next();
                return top;
            }
            NWBDRY => {
                self.wordchrs();
                let s = self.newstate(nfa);
                noerrn!();
                self.word(BEHIND, lp, s);
                self.word(AHEAD, s, rp);
                let s = self.newstate(nfa);
                noerrn!();
                self.nonword(BEHIND, lp, s);
                self.nonword(AHEAD, s, rp);
                self.next();
                return top;
            }
            LACON => {
                let latype = self.nextvalue as u8;
                self.next();
                let s = self.newstate(nfa);
                let s2 = self.newstate(nfa);
                noerrn!();
                let t = self.parse(b')' as i32, LACON, s, s2);
                // its inner structure does not matter
                self.freesubre(t);
                noerrn!();
                self.next();
                self.processlacon(s, s2, latype, lp, rp);
                return top;
            }
            // then the errors
            t if t == b'*' as i32 || t == b'+' as i32 || t == b'?' as i32 || t == b'{' as i32 => {
                self.verr(REG_BADRPT);
                return top;
            }
            // then plain characters, and their variants
            t if t == b')' as i32 || t == PLAIN => {
                if t == b')' as i32 {
                    // an unbalanced parenthesis
                    if (self.cflags & REG_ADVANCED) != REG_EXTENDED {
                        self.verr(REG_EPAREN);
                        return top;
                    }
                    // legal in EREs, by the specification's botch
                    self.note(REG_UPBOTCH);
                }
                let c = self.nextvalue;
                self.onechr(c, lp, rp);
                self.okcolors(nfa);
                noerrn!();
                self.next();
            }
            t if t == b'[' as i32 => {
                if self.nextvalue == 1 {
                    self.bracket(lp, rp);
                } else {
                    self.cbracket(lp, rp);
                }
                self.next();
            }
            CCLASSS => {
                let cls = cclass_of(self.nextvalue);
                self.charclass(cls, lp, rp);
                self.okcolors(nfa);
                self.next();
            }
            CCLASSC => {
                let cls = cclass_of(self.nextvalue);
                // charclasscomplement settles its colors itself
                self.charclasscomplement(cls, lp, rp);
                self.next();
            }
            t if t == b'.' as i32 => {
                let but = if (self.cflags & REG_NLSTOP) != 0 {
                    self.nlcolor
                } else {
                    COLORLESS
                };
                self.rainbow(nfa, PLAIN, but, lp, rp);
                self.next();
            }
            // and finally the hard ones
            t if t == b'(' as i32 => {
                // capturing or not
                let cap = typ != LACON && self.nextvalue != 0;
                if cap {
                    self.nsubexp += 1;
                    subno = self.nsubexp;
                    if subno as usize >= self.nsubs {
                        self.moresubs(subno);
                    }
                } else {
                    // anything but '('
                    atomtype = PLAIN;
                }
                self.next();

                // states of its own at each end, so that a back reference copies this sub-NFA
                // and nothing around it
                let s = self.newstate(nfa);
                let s2 = self.newstate(nfa);
                noerrn!();
                self.emptyarc(lp, s);
                self.emptyarc(s2, rp);
                noerrn!();
                atom = self.parse(b')' as i32, typ, s, s2);
                self.next();
                noerrn!();
                if cap {
                    if sr!(self, atom).capno == 0 {
                        // the usual case: the atom marked as capturing
                        sr!(self, atom).flags |= CAP;
                        sr!(self, atom).capno = subno;
                    } else {
                        // a wrapper that captures, for "((x))"
                        let f = sr!(self, atom).flags as i32 | CAP as i32;
                        let t = self.subre(b'(', f, s, s2);
                        noerrn!();
                        sr!(self, t).capno = subno;
                        sr!(self, t).child = atom;
                        atom = t;
                    }
                    self.subs[subno as usize] = atom;
                }
                // everything else waits on a possible {0}
            }
            BACKREF => {
                if typ == LACON {
                    self.verr(REG_ESUBREG);
                }
                subno = self.nextvalue as i32;
                if (subno as isize as usize) >= self.nsubs {
                    self.verr(REG_ESUBREG);
                }
                noerrn!();
                if self.subs[subno as usize] == NOSR {
                    self.verr(REG_ESUBREG);
                }
                noerrn!();
                atom = self.subre(b'b', BACKR as i32, lp, rp);
                noerrn!();
                sr!(self, atom).backno = subno;
                let target = self.subs[subno as usize];
                sr!(self, target).flags |= BRUSE;
                // for now, so that there is something
                self.emptyarc(lp, rp);
                self.next();
            }
            _ => {
                self.verr(REG_ASSERT);
                return top;
            }
        }

        // ...and an atom may be followed by a quantifier
        let m: i32;
        let n: i32;
        let qprefer: i32;
        let nt = self.nexttype;
        let greedy = |v: Chr| if v != 0 { LONGER } else { SHORTER } as i32;
        if nt == b'*' as i32 {
            m = 0;
            n = DUPINF;
            qprefer = greedy(self.nextvalue);
            self.next();
        } else if nt == b'+' as i32 {
            m = 1;
            n = DUPINF;
            qprefer = greedy(self.nextvalue);
            self.next();
        } else if nt == b'?' as i32 {
            m = 0;
            n = 1;
            qprefer = greedy(self.nextvalue);
            self.next();
        } else if nt == b'{' as i32 {
            self.next();
            m = self.scannum();
            if self.eat(b',' as i32) {
                n = if self.see(DIGIT) {
                    self.scannum()
                } else {
                    DUPINF
                };
                if m > n {
                    self.verr(REG_BADBR);
                    return top;
                }
                // {m,n} has a preference, even as {m,m}
                qprefer = greedy(self.nextvalue);
            } else {
                n = m;
                // {m} passes the operand's preference through
                qprefer = 0;
            }
            if !self.see(b'}' as i32) {
                // errors too
                self.verr(REG_BADBR);
                return top;
            }
            self.next();
        } else {
            // no quantifier
            m = 1;
            n = 1;
            qprefer = 0;
        }

        // {0} or {0,0} cancels everything
        if m == 0 && n == 0 {
            // a capture inside may still be referred to, so it is only unlinked
            if atom != NOSR && (sr!(self, atom).flags & CAP) != 0 {
                let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
                self.delsub(nfa, lp, ab);
                self.delsub(nfa, ae, rp);
            } else {
                if atom != NOSR {
                    self.freesubre(atom);
                }
                self.delsub(nfa, lp, rp);
            }
            self.emptyarc(lp, rp);
            return top;
        }

        // the simple case
        let atomflags = if atom != NOSR {
            sr!(self, atom).flags as i32
        } else {
            0
        };
        let f = sr!(self, top).flags as i32 | qprefer | atomflags;
        if atomtype != b'(' as i32 && atomtype != BACKREF && !messy(up(f)) {
            if !(m == 1 && n == 1) {
                self.repeat(lp, rp, m, n);
            }
            if atom != NOSR {
                self.freesubre(atom);
            }
            sr!(self, top).flags = f as u8;
            return top;
        }

        // the hard part: capturing parentheses, a back reference, a clash of preferences, or an
        // atom with one of those inside

        // the contents need a node even where they are plain
        if atom == NOSR {
            atom = self.subre(b'=', 0, lp, rp);
            noerrn!();
        }

        // the atom needs begin and end states of its own, apart from lp and rp
        if sr!(self, atom).begin == lp || sr!(self, atom).end == rp {
            let s = self.newstate(nfa);
            let s2 = self.newstate(nfa);
            noerrn!();
            self.moveouts(nfa, lp, s);
            self.moveins(nfa, rp, s2);
            sr!(self, atom).begin = s;
            sr!(self, atom).end = s2;
        } else {
            // its own already, unlinked from lp and rp for now
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.delsub(nfa, lp, ab);
            self.delsub(nfa, ae, rp);
        }

        // the skeleton: [lp] -> [s] -prefix-> -atom-> -rest-> [rp], or with an iteration,
        // [lp] -> [s] -iterator-> [s2] -rest-> [rp]
        let s = self.newstate(nfa);
        noerrn!();
        self.emptyarc(lp, s);
        noerrn!();

        // the rest of the branch split into x{...} and what follows
        let aflags = sr!(self, atom).flags as i32;
        let mut t = self.subre(b'.', combine_flags(qprefer, aflags), lp, rp);
        noerrn!();
        sr!(self, t).child = atom;
        // the node whose child is the atom
        let atomp = t;

        // top made the concatenation of the prefix and the rest
        let (tb, tf) = (sr!(self, top).begin, sr!(self, top).flags as i32);
        let prefix = self.subre(b'=', tf, tb, lp);
        sr!(self, top).child = prefix;
        noerrn!();
        sr!(self, top).op = b'.';
        sr!(self, prefix).sibling = t;

        // a back reference copies its sub-NFA now
        if atomtype == BACKREF {
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.delsub(nfa, ab, ae);
            let target = self.subs[subno as usize];
            let (rb, re) = (sr!(self, target).begin, sr!(self, target).end);
            // the recursion waited for this: a back reference may copy the skeleton
            self.dupnfa(nfa, rb, re, ab, ae);
            noerrn!();
            // a back reference's NFA enforces no constraints
            self.removeconstraints(nfa, ab, ae);
            noerrn!();
        }

        // the quantifier
        let s2: StateId;
        if atomtype == BACKREF {
            // a back reference repeats itself
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.emptyarc(s, ab);
            self.repeat(ab, ae, m, n);
            sr!(self, atom).min = m as i16;
            sr!(self, atom).max = n as i16;
            let af = sr!(self, atom).flags as i32;
            sr!(self, atom).flags |= combine_flags(qprefer, af) as u8;
            s2 = ae;
        } else if m == 1
            && n == 1
            && (qprefer == 0
                || (aflags & (LONGER | SHORTER | MIXED) as i32) == 0
                || qprefer == (aflags & (LONGER | SHORTER | MIXED) as i32))
        {
            // no quantifier, or one that changes nothing
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.emptyarc(s, ab);
            s2 = ae;
        } else if (aflags & (CAP | BACKR) as i32) == 0 {
            // with no captures nor back references inside, a plain node will do
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.emptyarc(s, ab);
            self.repeat(ab, ae, m, n);
            let f = combine_flags(qprefer, aflags);
            let tt = self.subre(b'=', f, ab, ae);
            noerrn!();
            self.freesubre(atom);
            sr!(self, atomp).child = tt;
            s2 = sr!(self, tt).end;
        } else if m > 0 && (aflags & BACKR as i32) == 0 {
            // x{m,n} as x{m-1,n-1}x, capturing in the last x only
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            self.dupnfa(nfa, ab, ae, s, ab);
            self.repeat(s, ab, m - 1, if n == DUPINF { n } else { n - 1 });
            let f = combine_flags(qprefer, aflags);
            let tt = self.subre(b'.', f, s, ae);
            noerrn!();
            let c = self.subre(b'=', pref(f), s, ab);
            sr!(self, tt).child = c;
            noerrn!();
            sr!(self, c).sibling = atom;
            sr!(self, atomp).child = tt;
            s2 = ae;
        } else {
            // the general case: an iteration node
            let (ab, ae) = (sr!(self, atom).begin, sr!(self, atom).end);
            let ns2 = self.newstate(nfa);
            noerrn!();
            self.moveouts(nfa, ae, ns2);
            noerrn!();
            self.dupnfa(nfa, ab, ae, s, ns2);
            self.repeat(s, ns2, m, n);
            let f = combine_flags(qprefer, aflags);
            let tt = self.subre(b'*', f, s, ns2);
            noerrn!();
            sr!(self, tt).min = m as i16;
            sr!(self, tt).max = n as i16;
            sr!(self, tt).child = atom;
            sr!(self, atomp).child = tt;
            // the rest of the branch hangs from the iteration's end
            s2 = ns2;
        }

        // and the recursion that waited
        t = sr!(self, sr!(self, top).child).sibling;
        if !(self.see(b'|' as i32) || self.see(stopper) || self.see(EOS)) {
            // the rest of the branch, as the second child of t's child
            let rest = self.parsebranch(stopper, typ, s2, rp, true);
            let tc = sr!(self, t).child;
            sr!(self, tc).sibling = rest;
            noerrn!();

            // the flags, as promised
            let rf = sr!(self, rest).flags as i32;
            let tf = sr!(self, t).flags as i32;
            sr!(self, t).flags |= combine_flags(tf, rf) as u8;
            let tf = sr!(self, t).flags as i32;
            let topf = sr!(self, top).flags as i32;
            sr!(self, top).flags |= combine_flags(topf, tf) as u8;

            // top and t are concatenations: top's child the branch's prefix, its sibling t; t's
            // child the atom, its sibling the rest. Where the atom came first, the prefix is
            // empty and one concatenation goes.
            let topc = sr!(self, top).child;
            if sr!(self, topc).begin == sr!(self, topc).end {
                self.freesubre(topc);
                sr!(self, top).child = sr!(self, t).child;
                self.freesrnode(t);
            } else {
                // otherwise an atom messy only by its preference may join the rest in one plain
                // node
                let tc = sr!(self, t).child;
                let tcs = sr!(self, tc).sibling;
                if sr!(self, tc).op == b'='
                    && sr!(self, tcs).op == b'='
                    && !messy(up(sr!(self, tc).flags as i32 | sr!(self, tcs).flags as i32))
                {
                    sr!(self, t).op = b'=';
                    let f = combine_flags(sr!(self, tc).flags as i32, sr!(self, tcs).flags as i32);
                    sr!(self, t).flags = f as u8;
                    self.freesubreandsiblings(tc);
                    sr!(self, t).child = NOSR;
                }
            }
        } else {
            // nothing left in the branch: s2 links straight to rp, and t goes
            self.emptyarc(s2, rp);
            let topc = sr!(self, top).child;
            sr!(self, topc).sibling = sr!(self, t).child;
            let sf = sr!(self, sr!(self, t).child).flags as i32;
            let topf = sr!(self, top).flags as i32;
            sr!(self, top).flags |= combine_flags(topf, sf) as u8;
            self.freesrnode(t);

            // where the messy atom was all the branch, top goes too
            if sr!(self, topc).begin == sr!(self, topc).end {
                let tt = sr!(self, topc).sibling;
                sr!(self, topc).sibling = NOSR;
                self.freesubre(top);
                top = tt;
            }
        }
        top
    }

    /// The arcs for a character ahead or behind that is not a word's (`nonword`).
    fn nonword(&mut self, dir: i32, lp: StateId, rp: StateId) {
        let anchor = if dir == AHEAD { b'$' } else { b'^' } as i32;
        let nfa = self.nfa;
        self.newarc(nfa, anchor, 1, lp, rp);
        self.newarc(nfa, anchor, 0, lp, rp);
        let wc = self.wordchrs;
        self.colorcomplement(nfa, dir, wc, lp, rp);
    }

    /// The arcs for a word's character ahead or behind (`word`).
    fn word(&mut self, dir: i32, lp: StateId, rp: StateId) {
        let nfa = self.nfa;
        let wc = self.wordchrs;
        self.cloneouts(nfa, wc, lp, rp, dir);
    }

    /// The arcs for a character class, alone or in a bracket; the caller settles the colors
    /// (`charclass`).
    fn charclass(&mut self, cls: CharClass, lp: StateId, rp: StateId) {
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(cls, (self.cflags & REG_ICASE) != 0);
        if self.iserr() {
            return;
        }
        self.subcolorcvec(&cv, lp, rp);
    }

    /// The arcs for a class's complement, alone or in a bracket with no colors open
    /// (`charclasscomplement`).
    fn charclasscomplement(&mut self, cls: CharClass, lp: StateId, rp: StateId) {
        let nfa = self.nfa;
        // a state to hang the class's arcs on for now
        let cstate = self.newstate(nfa);
        if self.iserr() {
            return;
        }
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(cls, (self.cflags & REG_ICASE) != 0);
        if self.iserr() {
            return;
        }
        self.subcolorcvec(&cv, cstate, cstate);
        if self.iserr() {
            return;
        }
        self.okcolors(nfa);
        if self.iserr() {
            return;
        }
        self.colorcomplement(nfa, PLAIN, cstate, lp, rp);
        if self.iserr() {
            return;
        }
        self.dropstate(nfa, cstate);
    }

    /// A bound's number, at most DUPMAX (`scannum`).
    fn scannum(&mut self) -> i32 {
        let mut n: i32 = 0;
        while self.see(DIGIT) && n < DUPMAX {
            n = n * 10 + self.nextvalue as i32;
            self.next();
        }
        if self.see(DIGIT) || n > DUPMAX {
            self.verr(REG_BADBR);
            return 0;
        }
        n
    }

    /// The sub-NFA between `lp` and `rp` made `m` to `n` repetitions of itself (`repeat`).
    fn repeat(&mut self, lp: StateId, rp: StateId, m: i32, n: i32) {
        const SOME: i32 = 2;
        const INF: i32 = 3;
        let reduce = |x: i32| {
            if x == DUPINF {
                INF
            } else if x > 1 {
                SOME
            } else {
                x
            }
        };
        let (rm, rn) = (reduce(m), reduce(n));
        let nfa = self.nfa;
        match (rm, rn) {
            (0, 0) => {
                // the empty string
                self.delsub(nfa, lp, rp);
                self.emptyarc(lp, rp);
            }
            (0, 1) => {
                // as x|
                self.emptyarc(lp, rp);
            }
            (0, SOME) => {
                // as x{1,n}|
                self.repeat(lp, rp, 1, n);
                if self.iserr() {
                    return;
                }
                self.emptyarc(lp, rp);
            }
            (0, INF) => {
                // x looped around
                let s = self.newstate(nfa);
                if self.iserr() {
                    return;
                }
                self.moveouts(nfa, lp, s);
                self.moveins(nfa, rp, s);
                self.emptyarc(lp, s);
                self.emptyarc(s, rp);
            }
            (1, 1) => {}
            (1, SOME) => {
                // as x{0,n-1}x = (x{1,n-1}|)x
                let s = self.newstate(nfa);
                if self.iserr() {
                    return;
                }
                self.moveouts(nfa, lp, s);
                self.dupnfa(nfa, s, rp, lp, s);
                if self.iserr() {
                    return;
                }
                self.repeat(lp, s, 1, n - 1);
                if self.iserr() {
                    return;
                }
                self.emptyarc(lp, s);
            }
            (1, INF) => {
                // a loop back
                let s = self.newstate(nfa);
                let s2 = self.newstate(nfa);
                if self.iserr() {
                    return;
                }
                self.moveouts(nfa, lp, s);
                self.moveins(nfa, rp, s2);
                self.emptyarc(lp, s);
                self.emptyarc(s2, rp);
                self.emptyarc(s2, s);
            }
            (SOME, SOME) => {
                // as x{m-1,n-1}x
                let s = self.newstate(nfa);
                if self.iserr() {
                    return;
                }
                self.moveouts(nfa, lp, s);
                self.dupnfa(nfa, s, rp, lp, s);
                if self.iserr() {
                    return;
                }
                self.repeat(lp, s, m - 1, n - 1);
            }
            (SOME, INF) => {
                // as x{m-1,}x
                let s = self.newstate(nfa);
                if self.iserr() {
                    return;
                }
                self.moveouts(nfa, lp, s);
                self.dupnfa(nfa, s, rp, lp, s);
                if self.iserr() {
                    return;
                }
                self.repeat(lp, s, m - 1, n);
            }
            _ => self.verr(REG_ASSERT),
        }
    }

    /// A bracket expression, not complemented (`bracket`). Complemented classes such as `\W`
    /// inside it wait for its end.
    fn bracket(&mut self, lp: StateId, rp: StateId) {
        let mut have_cclassc = [false; NUM_CCLASSES];
        self.next();
        while !self.see(b']' as i32) && !self.see(EOS) {
            self.brackpart(lp, rp, &mut have_cclassc);
        }

        // the open subcolors of the bracket's elements closed
        let nfa = self.nfa;
        self.okcolors(nfa);
        if self.iserr() {
            return;
        }

        // then the complemented elements
        let mut any_cclassc = false;
        for (i, have) in have_cclassc.iter().enumerate() {
            if *have {
                self.charclasscomplement(cclass_of(i as Chr), lp, rp);
                if self.iserr() {
                    return;
                }
                any_cclassc = true;
            }
        }

        // only a complemented element can make the bracket every color
        if any_cclassc {
            self.optimizebracket(lp, rp);
        }
    }

    /// A complemented bracket expression: the bracket between states of its own, then its
    /// complement (`cbracket`).
    fn cbracket(&mut self, lp: StateId, rp: StateId) {
        let nfa = self.nfa;
        let left = self.newstate(nfa);
        let right = self.newstate(nfa);
        if self.iserr() {
            return;
        }
        self.bracket(left, right);

        // under NLSTOP, newline is never in the complement
        if (self.cflags & REG_NLSTOP) != 0 {
            let nl = self.nlcolor;
            self.newarc(nfa, PLAIN, nl, left, right);
        }
        if self.iserr() {
            return;
        }

        self.colorcomplement(nfa, PLAIN, left, lp, rp);
        if self.iserr() {
            return;
        }
        self.dropstate(nfa, left);
        self.freestate(nfa, right);
    }

    /// One element of a bracket expression, or a range (`brackpart`).
    fn brackpart(&mut self, lp: StateId, rp: StateId, have_cclassc: &mut [bool; NUM_CCLASSES]) {
        let startc: Chr;
        let nt = self.nexttype;
        match nt {
            RANGE => {
                // a-b-c or another botch
                self.verr(REG_ERANGE);
                return;
            }
            PLAIN => {
                startc = self.nextvalue;
                self.next();
                // an ordinary character, not a range
                if !self.see(RANGE) {
                    self.onechr(startc, lp, rp);
                    return;
                }
                if self.iserr() {
                    return;
                }
            }
            COLLEL => {
                let startp = self.now;
                let endp = self.scanplain();
                if startp >= endp {
                    self.verr(REG_ECOLLATE);
                }
                if self.iserr() {
                    return;
                }
                let name = self.pattern[startp..endp].to_vec();
                startc = self.element(&name);
                if self.iserr() {
                    return;
                }
            }
            ECLASS => {
                let startp = self.now;
                let endp = self.scanplain();
                if startp >= endp {
                    self.verr(REG_ECOLLATE);
                }
                if self.iserr() {
                    return;
                }
                let name = self.pattern[startp..endp].to_vec();
                let c = self.element(&name);
                if self.iserr() {
                    return;
                }
                let cv = self.eclass(c, (self.cflags & REG_ICASE) != 0);
                if self.iserr() {
                    return;
                }
                self.subcolorcvec(&cv, lp, rp);
                return;
            }
            CCLASS => {
                let startp = self.now;
                let endp = self.scanplain();
                if startp >= endp {
                    self.verr(REG_ECTYPE);
                }
                if self.iserr() {
                    return;
                }
                let name = self.pattern[startp..endp].to_vec();
                let cls = self.lookupcclass(&name);
                if self.iserr() {
                    return;
                }
                self.charclass(cls, lp, rp);
                return;
            }
            CCLASSS => {
                let cls = cclass_of(self.nextvalue);
                self.charclass(cls, lp, rp);
                self.next();
                return;
            }
            CCLASSC => {
                // charclasscomplement waits for the bracket's end
                have_cclassc[self.nextvalue as usize] = true;
                self.next();
                return;
            }
            _ => {
                self.verr(REG_ASSERT);
                return;
            }
        }

        let endc: Chr;
        if self.see(RANGE) {
            self.next();
            match self.nexttype {
                PLAIN | RANGE => {
                    endc = self.nextvalue;
                    self.next();
                    if self.iserr() {
                        return;
                    }
                }
                COLLEL => {
                    let startp = self.now;
                    let endp = self.scanplain();
                    if startp >= endp {
                        self.verr(REG_ECOLLATE);
                    }
                    if self.iserr() {
                        return;
                    }
                    let name = self.pattern[startp..endp].to_vec();
                    endc = self.element(&name);
                    if self.iserr() {
                        return;
                    }
                }
                _ => {
                    self.verr(REG_ERANGE);
                    return;
                }
            }
        } else {
            endc = startc;
        }

        // ranges are unportable
        if startc != endc {
            self.note(REG_UUNPORT);
        }
        let cv = self.range(startc, endc, (self.cflags & REG_ICASE) != 0);
        if self.iserr() {
            return;
        }
        if let Some(cv) = cv {
            self.subcolorcvec(&cv, lp, rp);
        }
    }

    /// The plain characters of `[.`, `[=` or `[:` scanned: where they end (`scanplain`).
    fn scanplain(&mut self) -> usize {
        self.next();
        let mut endp = self.now;
        while self.see(PLAIN) {
            endp = self.now;
            self.next();
        }
        self.next();
        endp
    }

    /// The arcs for a plain character, and its other cases where case is ignored (`onechr`).
    fn onechr(&mut self, c: Chr, lp: StateId, rp: StateId) {
        if (self.cflags & REG_ICASE) == 0 {
            let mut lastsubcolor = COLORLESS;
            self.subcoloronechr(c, lp, rp, &mut lastsubcolor);
            return;
        }
        let cv = self.allcases(c);
        self.subcolorcvec(&cv, lp, rp);
    }

    /// A bracket's arcs made one RAINBOW arc where they have every color, as `[\s\S]`
    /// (`optimizebracket`).
    fn optimizebracket(&mut self, lp: StateId, rp: StateId) {
        // the colors of lp's arcs marked
        let mut a = st!(self, lp).outs;
        while a != NOARC {
            let co = ar!(self, a).co;
            self.cm.cd[co as usize].flags |= COLMARK;
            a = ar!(self, a).outchain;
        }

        // the marks cleared, and every live color checked
        let mut israinbow = true;
        for co in 0..=self.cm.max {
            let cd = &mut self.cm.cd[co];
            if (cd.flags & COLMARK) != 0 {
                cd.flags &= !COLMARK;
            } else if (cd.flags & FREECOL) == 0 && (cd.flags & PSEUDO) == 0 {
                israinbow = false;
            }
        }
        if !israinbow {
            return;
        }

        // the arcs replaced by a rainbow
        let nfa = self.nfa;
        loop {
            let a = st!(self, lp).outs;
            if a == NOARC {
                break;
            }
            self.freearc(nfa, a);
        }
        self.newarc(nfa, PLAIN, RAINBOW, lp, rp);
    }

    /// The word characters as arcs looping on a state of their own, once, for the word
    /// constraints (`wordchrs`). No subcolors may be open.
    fn wordchrs(&mut self) {
        if self.wordchrs != NOSTATE {
            return;
        }
        let nfa = self.nfa;
        let cstate = self.newstate(nfa);
        if self.iserr() {
            return;
        }
        self.note(REG_ULOCALE);
        let cv = self.cclasscvec(CharClass::Word, (self.cflags & REG_ICASE) != 0);
        if self.iserr() {
            return;
        }
        self.subcolorcvec(&cv, cstate, cstate);
        if self.iserr() {
            return;
        }
        self.okcolors(nfa);
        if self.iserr() {
            return;
        }
        self.wordchrs = cstate;
    }

    /// A lookaround constraint in the NFA: a LACON arc and its sub-NFA, or where the constraint
    /// is one character of a set, an AHEAD or BEHIND arc (`processlacon`).
    fn processlacon(&mut self, begin: StateId, end: StateId, latype: u8, lp: StateId, rp: StateId) {
        let nfa = self.nfa;
        let s1 = self.single_color_transition(begin, end);
        match latype {
            LATYPE_AHEAD_POS => {
                // colorset C ahead: AHEAD(C)
                if s1 != NOSTATE {
                    self.cloneouts(nfa, s1, lp, rp, AHEAD);
                    return;
                }
            }
            LATYPE_AHEAD_NEG => {
                // not colorset C ahead: AHEAD(^C)|$
                if s1 != NOSTATE {
                    self.colorcomplement(nfa, AHEAD, s1, lp, rp);
                    self.newarc(nfa, b'$' as i32, 1, lp, rp);
                    self.newarc(nfa, b'$' as i32, 0, lp, rp);
                    return;
                }
            }
            LATYPE_BEHIND_POS => {
                // colorset C behind: BEHIND(C)
                if s1 != NOSTATE {
                    self.cloneouts(nfa, s1, lp, rp, BEHIND);
                    return;
                }
            }
            LATYPE_BEHIND_NEG => {
                // not colorset C behind: BEHIND(^C)|^
                if s1 != NOSTATE {
                    self.colorcomplement(nfa, BEHIND, s1, lp, rp);
                    self.newarc(nfa, b'^' as i32, 1, lp, rp);
                    self.newarc(nfa, b'^' as i32, 0, lp, rp);
                    return;
                }
            }
            _ => {}
        }

        // the general case
        let n = self.newlacon(begin, end, latype);
        self.newarc(nfa, LACON, n as Color, lp, rp);
    }

    /// A new node of the tree (`subre`); the recursion of the parse is checked here.
    fn subre(&mut self, op: u8, flags: i32, begin: StateId, end: StateId) -> SrId {
        if self.stack_too_deep() {
            self.verr(REG_ETOOBIG);
            return NOSR;
        }
        let ret = if self.treefree != NOSR {
            let r = self.treefree;
            self.treefree = sr!(self, r).child;
            r
        } else {
            let r = self.srs.len() as SrId;
            self.srs.push(Sr::default());
            self.treechain = r;
            r
        };
        let node = &mut self.srs[ret as usize];
        node.op = op;
        node.flags = flags as u8;
        node.latype = 0xFF;
        node.id = 0;
        node.capno = 0;
        node.backno = 0;
        node.min = 1;
        node.max = 1;
        node.child = NOSR;
        node.sibling = NOSR;
        node.begin = begin;
        node.end = end;
        node.cnfa = Cnfa::default();
        ret
    }

    /// A node and its children freed, not its siblings (`freesubre`).
    pub(super) fn freesubre(&mut self, t: SrId) {
        if t == NOSR {
            return;
        }
        let child = sr!(self, t).child;
        if child != NOSR {
            self.freesubreandsiblings(child);
        }
        self.freesrnode(t);
    }

    /// A node, its children and its following siblings freed (`freesubreandsiblings`).
    fn freesubreandsiblings(&mut self, mut t: SrId) {
        while t != NOSR {
            let next = sr!(self, t).sibling;
            self.freesubre(t);
            t = next;
        }
    }

    /// One node freed: kept for reuse while the parse goes on (`freesrnode`).
    fn freesrnode(&mut self, t: SrId) {
        if t == NOSR {
            return;
        }
        let reuse = self.treechain != NOSR;
        let treefree = self.treefree;
        let node = &mut self.srs[t as usize];
        node.cnfa = Cnfa::default();
        node.flags = 0;
        node.child = NOSR;
        node.sibling = NOSR;
        node.begin = NOSTATE;
        node.end = NOSTATE;
        if reuse {
            node.child = treefree;
            self.treefree = t;
        }
    }

    /// Under REG_NOSUB, the captures no back reference refers to removed, and what that leaves
    /// plain made a plain node (`removecaptures`).
    fn removecaptures(&mut self, t: SrId) {
        if (sr!(self, t).flags & BRUSE) == 0 {
            sr!(self, t).capno = 0;
            sr!(self, t).flags &= !CAP;
        }
        let mut t2 = sr!(self, t).child;
        while t2 != NOSR {
            self.removecaptures(t2);
            if (sr!(self, t2).flags & CAP) != 0 {
                sr!(self, t).flags |= CAP;
            }
            t2 = sr!(self, t2).sibling;
        }
        if (sr!(self, t).flags & (CAP | BACKR)) == 0 {
            let child = sr!(self, t).child;
            if child != NOSR {
                self.freesubreandsiblings(child);
            }
            sr!(self, t).child = NOSR;
            sr!(self, t).op = b'=';
            sr!(self, t).flags &= !MIXED;
        }
    }

    /// The tree's nodes numbered from `start`, depth first: the next number (`numst`).
    fn numst(&mut self, t: SrId, start: i32) -> i32 {
        let mut i = start;
        sr!(self, t).id = i;
        i += 1;
        let mut t2 = sr!(self, t).child;
        while t2 != NOSR {
            i = self.numst(t2, i);
            t2 = sr!(self, t2).sibling;
        }
        i
    }

    /// The tree's nodes marked in use (`markst`).
    fn markst(&mut self, t: SrId) {
        sr!(self, t).flags |= INUSE;
        let mut t2 = sr!(self, t).child;
        while t2 != NOSR {
            self.markst(t2);
            t2 = sr!(self, t2).sibling;
        }
    }

    /// The nodes not in use let go, and the reuse of nodes ended (`cleanst`).
    fn cleanst(&mut self) {
        self.treechain = NOSR;
        self.treefree = NOSR;
    }

    /// Each node's compacted NFA, children first: what the top node's optimization noted
    /// (`nfatree`).
    fn nfatree(&mut self, t: SrId) -> i64 {
        let mut t2 = sr!(self, t).child;
        while t2 != NOSR {
            self.nfatree(t2);
            t2 = sr!(self, t2).sibling;
        }
        let (begin, end) = (sr!(self, t).begin, sr!(self, t).end);
        let (ret, cnfa) = self.nfanode(begin, end, false);
        if let Some(cnfa) = cnfa {
            sr!(self, t).cnfa = cnfa;
        }
        ret
    }

    /// The compacted NFA of the sub-NFA from `begin` to `end`, made a search's where
    /// `converttosearch`; with what its optimization noted (`nfanode`).
    pub(super) fn nfanode(
        &mut self,
        begin: StateId,
        end: StateId,
        converttosearch: bool,
    ) -> (i64, Option<Cnfa>) {
        let parent = self.nfa;
        let Some(nfa) = self.newnfa(Some(parent)) else {
            return (0, None);
        };
        let (init, fin) = (self.nfas[nfa].init, self.nfas[nfa].final_);
        self.dupnfa(nfa, begin, end, init, fin);
        self.nfas[nfa].flags = self.nfas[parent].flags;
        let mut ret = 0;
        let mut cnfa = None;
        if !self.iserr() {
            self.specialcolors(nfa);
        }
        if !self.iserr() {
            ret = self.optimize(nfa);
        }
        if converttosearch && !self.iserr() {
            self.makesearch(nfa);
        }
        if !self.iserr() {
            cnfa = self.compact(nfa);
        }
        self.freenfa(nfa);
        (ret, cnfa)
    }

    /// A new lookaround constraint: its number (`newlacon`).
    fn newlacon(&mut self, begin: StateId, end: StateId, latype: u8) -> i32 {
        let la = Lacon {
            begin: NOSTATE,
            end: NOSTATE,
            latype: 0,
            cnfa: Cnfa::default(),
        };
        let n = if self.nlacons == 0 {
            // the 0th is skipped
            self.lacons = vec![la.clone(), la];
            1
        } else {
            self.lacons.push(la);
            self.nlacons
        };
        self.nlacons = n + 1;
        let sub = &mut self.lacons[n as usize];
        sub.begin = begin;
        sub.end = end;
        sub.latype = latype;
        n
    }
}

/// The character class a token's value names.
pub(super) fn cclass_of(v: Chr) -> CharClass {
    use CharClass as K;
    const ALL: [CharClass; NUM_CCLASSES] = [
        K::Alnum,
        K::Alpha,
        K::Ascii,
        K::Blank,
        K::Cntrl,
        K::Digit,
        K::Graph,
        K::Lower,
        K::Print,
        K::Punct,
        K::Space,
        K::Upper,
        K::Xdigit,
        K::Word,
    ];
    ALL[v as usize]
}
