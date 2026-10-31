//! The colors (regc_color.c): the map from each character to its color, the colors split as each
//! atom names characters, and the arcs each color labels, kept on a chain per color so that a
//! split color's arcs gain their parallels.

use super::comp::Vars;
use super::cvec::Cvec;
use super::guts::*;
use super::locale::Ctype;
use super::nfa::{ar, st, ArcId, NfaId, StateId, NOARC};

// A color's flags.
pub(super) const FREECOL: i32 = 0o1;
pub(super) const PSEUDO: i32 = 0o2;
pub(super) const COLMARK: i32 = 0o4;

/// The value of `sub` with no open subcolor.
pub(super) const NOSUB: Color = COLORLESS;

/// How many color descriptors the map starts with (`NINLINECDS`).
const NINLINECDS: usize = 10;

/// One color while the pattern is compiled (`struct colordesc`).
#[derive(Clone, Copy, Debug)]
pub(super) struct Colordesc {
    /// Its characters up to MAX_SIMPLE_CHR, and its entries in the high map.
    pub(super) nschrs: i32,
    pub(super) nuchrs: i32,
    /// Its open subcolor, or the free chain's next.
    pub(super) sub: Color,
    /// The chain of its arcs.
    pub(super) arcs: ArcId,
    pub(super) firstchr: Chr,
    pub(super) flags: i32,
}

impl Default for Colordesc {
    fn default() -> Colordesc {
        Colordesc {
            nschrs: 0,
            nuchrs: 0,
            sub: NOSUB,
            arcs: NOARC,
            firstchr: CHR_MIN,
            flags: 0,
        }
    }
}

/// The color map while the pattern is compiled (`struct colormap`).
#[derive(Clone, Debug)]
pub(super) struct Cmap {
    /// How many descriptors are allocated, the greatest color in use, the free chain's first.
    pub(super) ncds: usize,
    pub(super) max: usize,
    pub(super) free: Color,
    pub(super) cd: Vec<Colordesc>,
    pub(super) locolormap: Vec<Color>,
    pub(super) classbits: [i32; NUM_CCLASSES],
    pub(super) cmranges: Vec<ColorMapRange>,
    pub(super) hicolormap: Vec<Color>,
    pub(super) maxarrayrows: i32,
    pub(super) hiarrayrows: i32,
    pub(super) hiarraycols: i32,
}

impl Cmap {
    /// A new map, every character WHITE (`initcm`).
    pub(super) fn initcm() -> Cmap {
        let mut cd = vec![Colordesc::default(); NINLINECDS];
        cd[WHITE as usize] = Colordesc {
            nschrs: (MAX_SIMPLE_CHR - CHR_MIN + 1) as i32,
            nuchrs: 1,
            sub: NOSUB,
            arcs: NOARC,
            firstchr: CHR_MIN,
            flags: 0,
        };
        Cmap {
            ncds: NINLINECDS,
            max: 0,
            free: 0,
            cd,
            locolormap: vec![WHITE; (MAX_SIMPLE_CHR - CHR_MIN + 1) as usize],
            classbits: [0; NUM_CCLASSES],
            cmranges: Vec::new(),
            // four rows allocated, one row and one column in use, all other characters WHITE
            hicolormap: vec![WHITE; 4],
            maxarrayrows: 4,
            hiarrayrows: 1,
            hiarraycols: 1,
        }
    }

    /// Whether a color is free (`UNUSEDCOLOR`).
    pub(super) fn unused(&self, co: usize) -> bool {
        (self.cd[co].flags & FREECOL) != 0
    }

    /// The map as the executor reads it.
    pub(super) fn runtime(&self, ctype: Ctype) -> ColorMap {
        let mut classbits = [0; NUM_CCLASSES];
        for (b, &c) in classbits.iter_mut().zip(&self.classbits) {
            *b = c as usize;
        }
        let used = (self.hiarrayrows * self.hiarraycols) as usize;
        ColorMap {
            locolormap: self.locolormap.clone(),
            classbits,
            cmranges: self.cmranges.clone(),
            hicolormap: self.hicolormap[..used].to_vec(),
            hiarraycols: self.hiarraycols as usize,
            pseudo: (0..=self.max)
                .map(|co| (self.cd[co].flags & PSEUDO) != 0)
                .collect(),
            ctype,
        }
    }
}

impl Vars {
    /// The greatest color in use, or COLORLESS after an error (`maxcolor`).
    pub(super) fn maxcolor(&self) -> Color {
        if self.iserr() {
            return COLORLESS;
        }
        self.cm.max as Color
    }

    /// A new color, to be given out at once (`newcolor`): COLORLESS after an error.
    pub(super) fn newcolor(&mut self) -> Color {
        if self.iserr() {
            return COLORLESS;
        }
        let cm = &mut self.cm;
        let co: usize;
        if cm.free != 0 {
            co = cm.free as usize;
            cm.free = cm.cd[co].sub;
        } else if cm.max < cm.ncds - 1 {
            cm.max += 1;
            co = cm.max;
        } else {
            // more descriptors
            if cm.max == MAX_COLOR as usize {
                self.verr(REG_ECOLORS);
                return COLORLESS;
            }
            let n = (cm.ncds * 2).min(MAX_COLOR as usize + 1);
            cm.cd.resize(n, Colordesc::default());
            cm.ncds = n;
            cm.max += 1;
            co = cm.max;
        }
        cm.cd[co] = Colordesc::default();
        co as Color
    }

    /// A color freed, which has no arcs nor subcolor (`freecolor`).
    pub(super) fn freecolor(&mut self, co: Color) {
        if co == WHITE {
            return;
        }
        let cm = &mut self.cm;
        cm.cd[co as usize].flags = FREECOL;

        if co as usize == cm.max {
            while cm.max > WHITE as usize && cm.unused(cm.max) {
                cm.max -= 1;
            }
            while cm.free as usize > cm.max {
                cm.free = cm.cd[cm.free as usize].sub;
            }
            if cm.free > 0 {
                let mut pco = cm.free;
                let mut nco = cm.cd[pco as usize].sub;
                while nco > 0 {
                    if nco as usize > cm.max {
                        // out of the free chain
                        nco = cm.cd[nco as usize].sub;
                        cm.cd[pco as usize].sub = nco;
                    } else {
                        pco = nco;
                        nco = cm.cd[pco as usize].sub;
                    }
                }
            }
        } else {
            cm.cd[co as usize].sub = cm.free;
            cm.free = co;
        }
    }

    /// A false color, kept by other means (`pseudocolor`).
    pub(super) fn pseudocolor(&mut self) -> Color {
        let co = self.newcolor();
        if self.iserr() {
            return COLORLESS;
        }
        self.cm.cd[co as usize] = Colordesc {
            nschrs: 0,
            // as though in the high map
            nuchrs: 1,
            sub: NOSUB,
            arcs: NOARC,
            firstchr: CHR_MIN,
            flags: PSEUDO,
        };
        co
    }

    /// A character up to MAX_SIMPLE_CHR given its color's open subcolor, made where needed
    /// (`subcolor`).
    pub(super) fn subcolor(&mut self, c: Chr) -> Color {
        let co = self.cm.locolormap[(c - CHR_MIN) as usize];
        let sco = self.newsub(co);
        if self.iserr() {
            return COLORLESS;
        }
        if co == sco {
            // in an open subcolor already
            return co;
        }
        let cm = &mut self.cm;
        cm.cd[co as usize].nschrs -= 1;
        if cm.cd[sco as usize].nschrs == 0 {
            cm.cd[sco as usize].firstchr = c;
        }
        cm.cd[sco as usize].nschrs += 1;
        cm.locolormap[(c - CHR_MIN) as usize] = sco;
        sco
    }

    /// An entry of the high map given its color's open subcolor, made where needed
    /// (`subcolorhi`).
    fn subcolorhi(&mut self, pco: usize) -> Color {
        let co = self.cm.hicolormap[pco];
        let sco = self.newsub(co);
        if self.iserr() {
            return COLORLESS;
        }
        if co == sco {
            return co;
        }
        let cm = &mut self.cm;
        cm.cd[co as usize].nuchrs -= 1;
        cm.cd[sco as usize].nuchrs += 1;
        cm.hicolormap[pco] = sco;
        sco
    }

    /// A color's open subcolor, made where it has none; a color of one character is its own
    /// (`newsub`).
    fn newsub(&mut self, co: Color) -> Color {
        let mut sco = self.cm.cd[co as usize].sub;
        if sco == NOSUB {
            // a color of one character need not be split
            let cd = &self.cm.cd[co as usize];
            if cd.nschrs + cd.nuchrs == 1 {
                return co;
            }
            sco = self.newcolor();
            if sco == COLORLESS {
                return COLORLESS;
            }
            self.cm.cd[co as usize].sub = sco;
            // an open subcolor points to itself
            self.cm.cd[sco as usize].sub = sco;
        }
        sco
    }

    /// A new row of the high map, a copy of `oldrow`: its number (`newhicolorrow`).
    fn newhicolorrow(&mut self, oldrow: i32) -> i32 {
        let cm = &mut self.cm;
        let newrow = cm.hiarrayrows;

        // a row's number, the storage doubled where needed
        if newrow >= cm.maxarrayrows {
            if cm.maxarrayrows >= i32::MAX / (cm.hiarraycols * 2) {
                self.verr(REG_ESPACE);
                return 0;
            }
            let n = (cm.maxarrayrows * 2 * cm.hiarraycols) as usize;
            cm.hicolormap.resize(n, WHITE);
            cm.maxarrayrows *= 2;
        }
        cm.hiarrayrows += 1;

        // the old row's colors, each counted again
        let cols = cm.hiarraycols as usize;
        let (o, n) = (oldrow as usize * cols, newrow as usize * cols);
        for i in 0..cols {
            let co = cm.hicolormap[o + i];
            cm.hicolormap[n + i] = co;
            cm.cd[co as usize].nuchrs += 1;
        }
        newrow
    }

    /// The high map's columns doubled, the new ones copies of the old (`newhicolorcols`).
    fn newhicolorcols(&mut self) {
        let cm = &mut self.cm;
        if cm.hiarraycols >= i32::MAX / (cm.maxarrayrows * 2) {
            self.verr(REG_ESPACE);
            return;
        }
        let cols = cm.hiarraycols as usize;
        let mut newarray = vec![WHITE; cm.maxarrayrows as usize * cols * 2];
        for r in (0..cm.hiarrayrows as usize).rev() {
            for c in 0..cols {
                let co = cm.hicolormap[r * cols + c];
                newarray[r * cols * 2 + c] = co;
                newarray[r * cols * 2 + cols + c] = co;
                cm.cd[co as usize].nuchrs += 1;
            }
        }
        cm.hicolormap = newarray;
        cm.hiarraycols *= 2;
    }

    /// Each character of a vector given its subcolor, and the arcs of those colors made from
    /// `lp` to `rp` (`subcolorcvec`).
    pub(super) fn subcolorcvec(&mut self, cv: &Cvec, lp: StateId, rp: StateId) {
        let nfa = self.nfa;
        let mut lastsubcolor = COLORLESS;

        // the characters
        for &ch in &cv.chrs {
            self.subcoloronechr(ch, lp, rp, &mut lastsubcolor);
            if self.iserr() {
                return;
            }
        }

        // and the ranges
        for &(from, to) in &cv.ranges {
            let mut from = from;
            if from <= MAX_SIMPLE_CHR {
                // the simple characters one at a time
                let lim = if to <= MAX_SIMPLE_CHR {
                    to
                } else {
                    MAX_SIMPLE_CHR
                };
                while from <= lim {
                    let sco = self.subcolor(from);
                    if self.iserr() {
                        return;
                    }
                    if sco != lastsubcolor {
                        self.newarc(nfa, super::comp::PLAIN, sco, lp, rp);
                        if self.iserr() {
                            return;
                        }
                        lastsubcolor = sco;
                    }
                    from += 1;
                }
            }
            // the part above MAX_SIMPLE_CHR
            if from < to {
                self.subcoloronerange(from, to, lp, rp, &mut lastsubcolor);
            } else if from == to {
                self.subcoloronechr(from, lp, rp, &mut lastsubcolor);
            }
            if self.iserr() {
                return;
            }
        }

        // and the class, if any
        if cv.cclasscode >= 0 {
            let code = cv.cclasscode as usize;
            // a column bit for the class, the map widened for it
            if self.cm.classbits[code] == 0 {
                self.cm.classbits[code] = self.cm.hiarraycols;
                self.newhicolorcols();
                if self.iserr() {
                    return;
                }
            }
            // each entry of the class's columns subcolored, its arc made
            let classbit = self.cm.classbits[code];
            let mut pco = 0;
            for _r in 0..self.cm.hiarrayrows {
                for c in 0..self.cm.hiarraycols {
                    if (c & classbit) != 0 {
                        let sco = self.subcolorhi(pco);
                        if self.iserr() {
                            return;
                        }
                        if sco != lastsubcolor {
                            self.newarc(nfa, super::comp::PLAIN, sco, lp, rp);
                            if self.iserr() {
                                return;
                            }
                            lastsubcolor = sco;
                        }
                    }
                    pco += 1;
                }
            }
        }
    }

    /// One character's part of `subcolorcvec`, low or high (`subcoloronechr`).
    pub(super) fn subcoloronechr(
        &mut self,
        ch: Chr,
        lp: StateId,
        rp: StateId,
        lastsubcolor: &mut Color,
    ) {
        let nfa = self.nfa;

        // a low character
        if ch <= MAX_SIMPLE_CHR {
            let sco = self.subcolor(ch);
            if self.iserr() {
                return;
            }
            if sco != *lastsubcolor {
                self.newarc(nfa, super::comp::PLAIN, sco, lp, rp);
                *lastsubcolor = sco;
            }
            return;
        }

        // up to two ranges more than now, where the character splits one
        let old = std::mem::take(&mut self.cm.cmranges);
        let mut newranges: Vec<ColorMapRange> = Vec::with_capacity(old.len() + 2);
        let mut i = 0;

        // the ranges before it, unchanged
        while i < old.len() {
            if old[i].cmax >= ch {
                break;
            }
            newranges.push(old[i]);
            i += 1;
        }

        // the range it falls in
        let newrow;
        if i >= old.len() || old[i].cmin > ch {
            // none: a new one, its row a copy of the "all others" row
            newrow = self.newhicolorrow(0);
            newranges.push(ColorMapRange {
                cmin: ch,
                cmax: ch,
                rownum: newrow as usize,
            });
        } else if old[i].cmin == old[i].cmax {
            // a range of this one character already
            newranges.push(old[i]);
            newrow = old[i].rownum as i32;
            i += 1;
        } else {
            // a range it must split
            let o = old[i];
            if ch > o.cmin {
                // the part before it
                newranges.push(ColorMapRange {
                    cmin: o.cmin,
                    cmax: ch - 1,
                    rownum: o.rownum,
                });
            }
            // the character alone, its row a copy of the range's
            newrow = self.newhicolorrow(o.rownum as i32);
            newranges.push(ColorMapRange {
                cmin: ch,
                cmax: ch,
                rownum: newrow as usize,
            });
            if ch < o.cmax {
                // the part after it, a row of its own where both parts are new
                let rownum = if ch > o.cmin {
                    self.newhicolorrow(o.rownum as i32) as usize
                } else {
                    o.rownum
                };
                newranges.push(ColorMapRange {
                    cmin: ch + 1,
                    cmax: o.cmax,
                    rownum,
                });
            }
            i += 1;
        }

        // the new row's colors, and their arcs
        self.subcoloronerow(newrow, lp, rp, lastsubcolor);

        // the ranges after it, unchanged
        newranges.extend_from_slice(&old[i..]);

        self.cm.cmranges = newranges;
    }

    /// A high range's part of `subcolorcvec` (`subcoloronerange`).
    fn subcoloronerange(
        &mut self,
        from: Chr,
        to: Chr,
        lp: StateId,
        rp: StateId,
        lastsubcolor: &mut Color,
    ) {
        let mut from = from;
        // as many as 2N+1 ranges from N
        let old = std::mem::take(&mut self.cm.cmranges);
        let mut newranges: Vec<ColorMapRange> = Vec::with_capacity(old.len() * 2 + 1);
        let mut i = 0;

        // the ranges before it, unchanged
        while i < old.len() {
            if old[i].cmax >= from {
                break;
            }
            newranges.push(old[i]);
            i += 1;
        }

        // the ranges it overlaps, `from` moved past each
        while i < old.len() && old[i].cmin <= to {
            let o = old[i];
            if from < o.cmin {
                // the part of the new range in no old one
                let newrow = self.newhicolorrow(0);
                newranges.push(ColorMapRange {
                    cmin: from,
                    cmax: o.cmin - 1,
                    rownum: newrow as usize,
                });
                self.subcoloronerow(newrow, lp, rp, lastsubcolor);
                from = o.cmin;
            }

            let newrow;
            if from <= o.cmin && to >= o.cmax {
                // the old range wholly inside the new
                newranges.push(o);
                newrow = o.rownum as i32;
                from = o.cmax + 1;
            } else {
                // part of the old range outside the new
                if from > o.cmin {
                    // the part before the new range
                    newranges.push(ColorMapRange {
                        cmin: o.cmin,
                        cmax: from - 1,
                        rownum: o.rownum,
                    });
                }
                // the common part, its row a copy of the old range's
                newrow = self.newhicolorrow(o.rownum as i32);
                newranges.push(ColorMapRange {
                    cmin: from,
                    cmax: if to < o.cmax { to } else { o.cmax },
                    rownum: newrow as usize,
                });
                if to < o.cmax {
                    // the part after the new range, a row of its own where both parts are new
                    let rownum = if from > o.cmin {
                        self.newhicolorrow(o.rownum as i32) as usize
                    } else {
                        o.rownum
                    };
                    newranges.push(ColorMapRange {
                        cmin: to + 1,
                        cmax: o.cmax,
                        rownum,
                    });
                }
                from = o.cmax + 1;
            }
            self.subcoloronerow(newrow, lp, rp, lastsubcolor);
            i += 1;
        }

        if from <= to {
            // the part of the new range in no old one
            let newrow = self.newhicolorrow(0);
            newranges.push(ColorMapRange {
                cmin: from,
                cmax: to,
                rownum: newrow as usize,
            });
            self.subcoloronerow(newrow, lp, rp, lastsubcolor);
        }

        // the ranges after it, unchanged
        newranges.extend_from_slice(&old[i..]);

        self.cm.cmranges = newranges;
    }

    /// One new row of the high map given its subcolors, their arcs made (`subcoloronerow`).
    fn subcoloronerow(&mut self, rownum: i32, lp: StateId, rp: StateId, lastsubcolor: &mut Color) {
        let nfa = self.nfa;
        let cols = self.cm.hiarraycols as usize;
        let mut pco = rownum as usize * cols;
        for _ in 0..cols {
            let sco = self.subcolorhi(pco);
            if self.iserr() {
                return;
            }
            if sco != *lastsubcolor {
                self.newarc(nfa, super::comp::PLAIN, sco, lp, rp);
                if self.iserr() {
                    return;
                }
                *lastsubcolor = sco;
            }
            pco += 1;
        }
    }

    /// The subcolors made full colors: an emptied color's arcs relabeled with its subcolor, a
    /// split color's arcs given parallels of the subcolor (`okcolors`).
    pub(super) fn okcolors(&mut self, nfa: NfaId) {
        let end = self.cm.max;
        for co in 0..=end {
            let cd = self.cm.cd[co];
            let sco = cd.sub;
            if (cd.flags & FREECOL) != 0 || sco == NOSUB {
                // no subcolor
            } else if sco as usize == co {
                // a subcolor, which its parent deals with
            } else if cd.nschrs == 0 && cd.nuchrs == 0 {
                // the parent is empty: its arcs become the subcolor's, and it is freed
                self.cm.cd[co].sub = NOSUB;
                self.cm.cd[sco as usize].sub = NOSUB;
                loop {
                    let a = self.cm.cd[co].arcs;
                    if a == NOARC {
                        break;
                    }
                    self.uncolorchain(a);
                    ar!(self, a).co = sco;
                    self.colorchain(a);
                }
                self.freecolor(co as Color);
            } else {
                // the parent's arcs gain parallel arcs of the subcolor
                self.cm.cd[co].sub = NOSUB;
                self.cm.cd[sco as usize].sub = NOSUB;
                let mut a = self.cm.cd[co].arcs;
                while a != NOARC {
                    let (t, from, to) = (ar!(self, a).typ, ar!(self, a).from, ar!(self, a).to);
                    self.newarc(nfa, t, sco, from, to);
                    a = ar!(self, a).colorchain;
                }
            }
        }
    }

    /// An arc put on its color's chain (`colorchain`).
    pub(super) fn colorchain(&mut self, a: ArcId) {
        let co = ar!(self, a).co as usize;
        let head = self.cm.cd[co].arcs;
        if head != NOARC {
            ar!(self, head).colorchain_rev = a;
        }
        ar!(self, a).colorchain = head;
        ar!(self, a).colorchain_rev = NOARC;
        self.cm.cd[co].arcs = a;
    }

    /// An arc taken off its color's chain (`uncolorchain`).
    pub(super) fn uncolorchain(&mut self, a: ArcId) {
        let co = ar!(self, a).co as usize;
        let aa = ar!(self, a).colorchain_rev;
        let next = ar!(self, a).colorchain;
        if aa == NOARC {
            self.cm.cd[co].arcs = next;
        } else {
            ar!(self, aa).colorchain = next;
        }
        if next != NOARC {
            ar!(self, next).colorchain_rev = aa;
        }
        ar!(self, a).colorchain = NOARC;
        ar!(self, a).colorchain_rev = NOARC;
    }

    /// Arcs of every full color but `but` between two states: one RAINBOW arc where there is no
    /// exception (`rainbow`).
    pub(super) fn rainbow(
        &mut self,
        nfa: NfaId,
        typ: i32,
        but: Color,
        from: StateId,
        to: StateId,
    ) {
        if but == COLORLESS {
            self.newarc(nfa, typ, RAINBOW, from, to);
            return;
        }

        // subcolors, pseudocolors and `but` skipped
        let end = self.cm.max;
        for co in 0..=end {
            if self.iserr() {
                break;
            }
            let cd = self.cm.cd[co];
            if (cd.flags & FREECOL) == 0
                && cd.sub as usize != co
                && co as Color != but
                && (cd.flags & PSEUDO) == 0
            {
                self.newarc(nfa, typ, co as Color, from, to);
            }
        }
    }

    /// Arcs of every color, but the pseudocolors and those of the PLAIN arcs out of `of`
    /// (`colorcomplement`).
    pub(super) fn colorcomplement(
        &mut self,
        nfa: NfaId,
        typ: i32,
        of: StateId,
        from: StateId,
        to: StateId,
    ) {
        // a RAINBOW arc leaves an empty complement: a CANTMATCH arc keeps the NFA connected
        if self.findarc(of, super::comp::PLAIN, RAINBOW) != NOARC {
            self.newarc(nfa, super::comp::CANTMATCH, 0, from, to);
            self.nfas[nfa].flags |= super::nfa::HASCANTMATCH;
            return;
        }

        // the colors of of's arcs marked for now
        let mut a = st!(self, of).outs;
        while a != NOARC {
            if ar!(self, a).typ == super::comp::PLAIN {
                let co = ar!(self, a).co as usize;
                self.cm.cd[co].flags |= COLMARK;
            }
            a = ar!(self, a).outchain;
        }

        // the marks cleared, the arcs of the colors not marked made
        let end = self.cm.max;
        for co in 0..=end {
            if self.iserr() {
                break;
            }
            let flags = self.cm.cd[co].flags;
            if (flags & COLMARK) != 0 {
                self.cm.cd[co].flags &= !COLMARK;
            } else if (flags & FREECOL) == 0 && (flags & PSEUDO) == 0 {
                self.newarc(nfa, typ, co as Color, from, to);
            }
        }
    }
}
