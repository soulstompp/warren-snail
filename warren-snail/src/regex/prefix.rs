//! The prefix every string a compiled expression matches begins with, as PostgreSQL 19's
//! `regprefix.c` finds it: read off the search NFA of the tree's top node, from its anchor at the
//! start, one singleton color a state, stopping where a state leads on by more than one.

use super::guts::*;

/// What a compiled expression says of the strings it matches (`pg_regprefix`'s return).
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Prefix {
    /// No prefix common to them (REG_NOMATCH).
    None,
    /// Every one begins with these characters (REG_PREFIX).
    Partial(Vec<Chr>),
    /// Every one is these characters (REG_EXACT).
    Exact(Vec<Chr>),
}

/// The arcs of a state of a compacted NFA, up to the one that ends them.
fn arcs(cnfa: &Cnfa, st: usize) -> impl Iterator<Item = &Carc> {
    cnfa.arcs[cnfa.states[st]..]
        .iter()
        .take_while(|a| a.co != COLORLESS)
}

/// `pg_regprefix`.
pub(super) fn regprefix(g: &Guts) -> Prefix {
    if g.info & REG_UIMPOSSIBLE != 0 {
        return Prefix::None;
    }
    // Constraints such as back references are not applied, which the result's contract allows.
    let cnfa = &g.tree().cnfa;
    if cnfa.flags & MATCHALL != 0 {
        return Prefix::None;
    }
    findprefix(cnfa, &g.cmap)
}

/// `findprefix`.
fn findprefix(cnfa: &Cnfa, cm: &ColorMap) -> Prefix {
    let bos = |co: Color| co == cnfa.bos[0] || co == cnfa.bos[1];
    let eos = |co: Color| co == cnfa.eos[0] || co == cnfa.eos[1];

    // The "pre" state must have only BOS/BOL outarcs, else the pattern isn't anchored left; both
    // BOS and BOL must go to the same next state.
    let mut nextst: Option<usize> = None;
    for ca in arcs(cnfa, cnfa.pre) {
        if !bos(ca.co) {
            return Prefix::None;
        }
        match nextst {
            None => nextst = Some(ca.to),
            Some(n) if n != ca.to => return Prefix::None,
            Some(_) => {}
        }
    }
    let Some(mut next) = nextst else {
        return Prefix::None;
    };

    // Successive states, stopping at one with more than one acceptable transition character: more
    // than one color on its plain outarcs, or a color with more than one member.
    let mut string: Vec<Chr> = Vec::new();
    let mut st;
    loop {
        st = next;
        let mut nextst: Option<usize> = None;
        let mut thiscolor = COLORLESS;
        for ca in arcs(cnfa, st) {
            if bos(ca.co) {
                continue;
            }
            // EOS/EOL arcs end the search, as do RAINBOW arcs and LACONs.
            if eos(ca.co) || ca.co == RAINBOW || ca.co as usize >= cnfa.ncolors {
                thiscolor = COLORLESS;
                break;
            }
            if thiscolor == COLORLESS {
                thiscolor = ca.co;
                nextst = Some(ca.to);
            } else if thiscolor == ca.co {
                nextst = None;
            } else {
                thiscolor = COLORLESS;
                break;
            }
        }
        if thiscolor == COLORLESS {
            break;
        }
        // The color must be a singleton, with no high-color-map entries.
        let cd = cm.cd[thiscolor as usize];
        if cd.nschrs != 1 || cd.nuchrs != 0 {
            break;
        }
        // Its first member, where that still has the color.
        let c = cd.firstchr;
        if cm.getcolor(c) != thiscolor {
            break;
        }
        string.push(c);
        match nextst {
            Some(n) => next = n,
            None => break,
        }
    }

    // A state whose only outarcs are EOS/EOL ones to the post state ends an exact match, even of
    // no characters.
    let mut nextst: Option<usize> = None;
    for ca in arcs(cnfa, st) {
        if !eos(ca.co) {
            nextst = None;
            break;
        }
        match nextst {
            None => nextst = Some(ca.to),
            Some(n) if n != ca.to => {
                nextst = None;
                break;
            }
            Some(_) => {}
        }
    }
    if nextst == Some(cnfa.post) {
        return Prefix::Exact(string);
    }
    if !string.is_empty() {
        return Prefix::Partial(string);
    }
    Prefix::None
}
