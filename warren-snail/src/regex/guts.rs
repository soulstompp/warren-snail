//! What a compiled regular expression holds, as PostgreSQL 19's `regguts.h` and `regex.h` lay it
//! out: the flags, the error codes and their messages, the compacted NFAs, the tree of
//! subexpressions, and the map from a character to its color. The compiler (`comp.rs`) makes a
//! `Guts`; the executor (`exec.rs`) reads one.

use super::locale::Ctype;

/// A character: a code point, as PostgreSQL's `pg_wchar` holds one.
pub type Chr = u32;
/// A color: the class of characters an arc matches.
pub type Color = i16;

pub const CHR_MIN: Chr = 0;
pub const CHR_MAX: Chr = 0x7fff_fffe;
/// The last character the color map holds in a plain array.
pub const MAX_SIMPLE_CHR: Chr = 0x7FF;

pub const MAX_COLOR: Color = 32767;
/// No color.
pub const COLORLESS: Color = -1;
/// Every color but the pseudocolors.
pub const RAINBOW: Color = -2;
/// The color every character has until it is given another.
pub const WHITE: Color = 0;

/// The greatest count a bounded quantifier takes, and the count that stands for no bound.
pub const DUPMAX: i32 = 255;
pub const DUPINF: i32 = DUPMAX + 1;

// The kinds of lookaround constraint.
pub const LATYPE_AHEAD_POS: u8 = 0o3;
pub const LATYPE_AHEAD_NEG: u8 = 0o2;
pub const LATYPE_BEHIND_POS: u8 = 0o1;
pub const LATYPE_BEHIND_NEG: u8 = 0o0;

pub fn latype_is_pos(la: u8) -> bool {
    la & 0o1 != 0
}

pub fn latype_is_ahead(la: u8) -> bool {
    la & 0o2 != 0
}

/// The character classes a bracket names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum CharClass {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    Word,
}

pub const NUM_CCLASSES: usize = 14;

// Compile flags (regex.h).
pub const REG_BASIC: i32 = 0o0;
pub const REG_EXTENDED: i32 = 0o1;
pub const REG_ADVF: i32 = 0o2;
pub const REG_ADVANCED: i32 = 0o3;
pub const REG_QUOTE: i32 = 0o4;
pub const REG_NOSPEC: i32 = REG_QUOTE;
pub const REG_ICASE: i32 = 0o10;
pub const REG_NOSUB: i32 = 0o20;
pub const REG_EXPANDED: i32 = 0o40;
pub const REG_NLSTOP: i32 = 0o100;
pub const REG_NLANCH: i32 = 0o200;
pub const REG_NEWLINE: i32 = 0o300;
pub const REG_EXPECT: i32 = 0o1000;
pub const REG_BOSONLY: i32 = 0o2000;
/// A test flag: `[[=x=]]` stands for x and y.
pub const REG_FAKE: i32 = 0o10000;

// Execution flags (regex.h).
pub const REG_NOTBOL: i32 = 0o1;
pub const REG_NOTEOL: i32 = 0o2;
pub const REG_SMALL: i32 = 0o40;

// What the compiler notes of a pattern (`re_info`).
pub const REG_UBACKREF: i64 = 0o1;
pub const REG_ULOOKAROUND: i64 = 0o2;
pub const REG_UBOUNDS: i64 = 0o4;
pub const REG_UBRACES: i64 = 0o10;
pub const REG_UBSALNUM: i64 = 0o20;
pub const REG_UPBOTCH: i64 = 0o40;
pub const REG_UBBS: i64 = 0o100;
pub const REG_UNONPOSIX: i64 = 0o200;
pub const REG_UUNSPEC: i64 = 0o400;
pub const REG_UUNPORT: i64 = 0o1000;
pub const REG_ULOCALE: i64 = 0o2000;
pub const REG_UEMPTYMATCH: i64 = 0o4000;
pub const REG_UIMPOSSIBLE: i64 = 0o10000;
pub const REG_USHORTEST: i64 = 0o20000;

// Error codes (regex.h).
pub const REG_OKAY: i32 = 0;
pub const REG_NOMATCH: i32 = 1;
pub const REG_BADPAT: i32 = 2;
pub const REG_ECOLLATE: i32 = 3;
pub const REG_ECTYPE: i32 = 4;
pub const REG_EESCAPE: i32 = 5;
pub const REG_ESUBREG: i32 = 6;
pub const REG_EBRACK: i32 = 7;
pub const REG_EPAREN: i32 = 8;
pub const REG_EBRACE: i32 = 9;
pub const REG_BADBR: i32 = 10;
pub const REG_ERANGE: i32 = 11;
pub const REG_ESPACE: i32 = 12;
pub const REG_BADRPT: i32 = 13;
pub const REG_ASSERT: i32 = 15;
pub const REG_INVARG: i32 = 16;
pub const REG_MIXED: i32 = 17;
pub const REG_BADOPT: i32 = 18;
pub const REG_ETOOBIG: i32 = 19;
pub const REG_ECOLORS: i32 = 20;

/// An error of the engine, by its code.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegError(pub i32);

impl RegError {
    /// The message `pg_regerror` gives for the code (`regerrs.h`).
    pub fn message(&self) -> String {
        match self.0 {
            REG_OKAY => "no errors detected".into(),
            REG_NOMATCH => "failed to match".into(),
            REG_BADPAT => "invalid regexp (reg version 0.8)".into(),
            REG_ECOLLATE => "invalid collating element".into(),
            REG_ECTYPE => "invalid character class".into(),
            REG_EESCAPE => "invalid escape \\ sequence".into(),
            REG_ESUBREG => "invalid backreference number".into(),
            REG_EBRACK => "brackets [] not balanced".into(),
            REG_EPAREN => "parentheses () not balanced".into(),
            REG_EBRACE => "braces {} not balanced".into(),
            REG_BADBR => "invalid repetition count(s)".into(),
            REG_ERANGE => "invalid character range".into(),
            REG_ESPACE => "out of memory".into(),
            REG_BADRPT => "quantifier operand invalid".into(),
            REG_ASSERT => "\"cannot happen\" -- you found a bug".into(),
            REG_INVARG => "invalid argument to regex function".into(),
            REG_MIXED => "character widths of regex and string differ".into(),
            REG_BADOPT => "invalid embedded option".into(),
            REG_ETOOBIG => "regular expression is too complex".into(),
            REG_ECOLORS => "too many colors".into(),
            n => format!("*** unknown regex error code 0x{n:x} ***"),
        }
    }
}

/// One arc of a compacted NFA: a plain arc's color, or a lookaround constraint's number plus the
/// NFA's `ncolors`; and the state it leads to. A state's arcs end in one whose color is
/// `COLORLESS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Carc {
    pub co: Color,
    pub to: usize,
}

// A compacted NFA's flags.
pub const HASLACONS: i32 = 0o1;
pub const MATCHALL: i32 = 0o2;
/// A state of a compacted NFA that makes no progress.
pub const CNFA_NOPROGRESS: u8 = 0o1;

/// A compacted NFA (`struct cnfa`). `nstates` 0 is the null one.
#[derive(Clone, Debug, Default)]
pub struct Cnfa {
    pub nstates: usize,
    /// The greatest color in use, plus one.
    pub ncolors: usize,
    pub flags: i32,
    /// The setup state and the teardown state.
    pub pre: usize,
    pub post: usize,
    /// The colors given to the start of the string and of a line, and to their ends: index 0 where
    /// the execution flag says the string's edge is not a line's (`REG_NOTBOL`, `REG_NOTEOL`),
    /// index 1 otherwise.
    pub bos: [Color; 2],
    pub eos: [Color; 2],
    /// Each state's flags.
    pub stflags: Vec<u8>,
    /// Where each state's arcs begin in `arcs`.
    pub states: Vec<usize>,
    /// Every state's arcs, each state's ended by one whose color is `COLORLESS`.
    pub arcs: Vec<Carc>,
    /// Where `flags` holds `MATCHALL`, the fewest and most characters it matches (`DUPINF` for no
    /// most); -1 otherwise.
    pub minmatchall: i32,
    pub maxmatchall: i32,
}

impl Cnfa {
    #[cfg(test)]
    pub fn is_null(&self) -> bool {
        self.nstates == 0
    }
}

// A subexpression's flags.
pub const LONGER: u8 = 0o1;
pub const SHORTER: u8 = 0o2;
pub const MIXED: u8 = 0o4;
pub const CAP: u8 = 0o10;
pub const BACKR: u8 = 0o20;
pub const BRUSE: u8 = 0o40;
pub const INUSE: u8 = 0o100;

/// One node of the tree of subexpressions, as the executor reads it (`struct subre`). `op` is one
/// of `=` (a plain expression, matched by its DFA), `b` (a back reference), `(` (a capture of its
/// one child), `.` (two children in turn), `|` (any of its children), `*` (its one child, some
/// number of times).
#[derive(Clone, Debug, Default)]
pub struct Subre {
    pub op: u8,
    pub flags: u8,
    /// The kind of lookaround constraint, in `Guts::lacons`.
    pub latype: u8,
    /// A capture's subexpression number.
    pub capno: usize,
    /// A back reference's subexpression number.
    pub backno: usize,
    /// An iteration's or a back reference's fewest and most repetitions (`DUPINF` for no most).
    pub min: i16,
    pub max: i16,
    /// The node's first child and its next sibling, by their numbers in the tree (`numst`), their
    /// places in `Guts::subres`.
    pub child: Option<usize>,
    pub sibling: Option<usize>,
    pub cnfa: Cnfa,
}

/// One range of characters above `MAX_SIMPLE_CHR` the pattern names: a row of the high color map.
#[derive(Clone, Copy, Debug)]
pub struct ColorMapRange {
    pub cmin: Chr,
    pub cmax: Chr,
    pub rownum: usize,
}

/// What the compiler knew of a color's characters (`struct colordesc`): how many of those up to
/// `MAX_SIMPLE_CHR` it holds, its entries in the high map, and the first character it was given.
#[derive(Clone, Copy, Debug, Default)]
pub struct ColorChrs {
    pub nschrs: i32,
    pub nuchrs: i32,
    pub firstchr: Chr,
}

/// The map from a character to its color, as the executor reads it (the run-time part of `struct
/// colormap`).
#[derive(Clone, Debug)]
pub struct ColorMap {
    /// The colors of the characters up to `MAX_SIMPLE_CHR`.
    pub locolormap: Vec<Color>,
    /// For each character class, the bit it adds to a high character's column, or 0 where the
    /// pattern does not ask the class.
    pub classbits: [usize; NUM_CCLASSES],
    /// The ranges above `MAX_SIMPLE_CHR` the pattern names, in order; a character in none is row 0.
    pub cmranges: Vec<ColorMapRange>,
    /// The high colors, row by row, `hiarraycols` a row.
    pub hicolormap: Vec<Color>,
    pub hiarraycols: usize,
    /// Which colors are pseudocolors, by color.
    pub pseudo: Vec<bool>,
    /// What the compiler knew of each color's characters, by color, as `pg_regprefix` reads it.
    pub cd: Vec<ColorChrs>,
    /// The character classes the colors were made under.
    pub ctype: Ctype,
}

impl ColorMap {
    /// The color of a character (`GETCOLOR`, `pg_reg_getcolor`).
    pub fn getcolor(&self, c: Chr) -> Color {
        if c <= MAX_SIMPLE_CHR {
            return self.locolormap[(c - CHR_MIN) as usize];
        }
        let mut rownum = 0;
        let (mut low, mut high) = (0, self.cmranges.len());
        while low < high {
            let middle = low + (high - low) / 2;
            let cmr = &self.cmranges[middle];
            if c < cmr.cmin {
                high = middle;
            } else if c > cmr.cmax {
                low = middle + 1;
            } else {
                rownum = cmr.rownum;
                break;
            }
        }
        if self.hiarraycols > 1 {
            let colnum = self.cclass_column_index(c);
            self.hicolormap[rownum * self.hiarraycols + colnum]
        } else {
            self.hicolormap[rownum]
        }
    }

    /// Whether a color is a pseudocolor, which no character has.
    pub fn is_pseudo(&self, co: Color) -> bool {
        co >= 0 && self.pseudo.get(co as usize).copied().unwrap_or(false)
    }

    /// A high character's column: the bits of the classes the pattern asks that it is in
    /// (`cclass_column_index`).
    pub fn cclass_column_index(&self, c: Chr) -> usize {
        use CharClass as C;
        let t = &self.ctype;
        let bit = |cls: C| self.classbits[cls as usize];
        let mut colnum = 0;
        if bit(C::Print) != 0 && t.isprint(c) {
            colnum |= bit(C::Print);
        }
        if bit(C::Alnum) != 0 && t.isalnum(c) {
            colnum |= bit(C::Alnum);
        }
        if bit(C::Alpha) != 0 && t.isalpha(c) {
            colnum |= bit(C::Alpha);
        }
        if bit(C::Word) != 0 && t.isword(c) {
            colnum |= bit(C::Word);
        }
        if bit(C::Digit) != 0 && t.isdigit(c) {
            colnum |= bit(C::Digit);
        }
        if bit(C::Punct) != 0 && t.ispunct(c) {
            colnum |= bit(C::Punct);
        }
        if bit(C::Space) != 0 && t.isspace(c) {
            colnum |= bit(C::Space);
        }
        if bit(C::Lower) != 0 && t.islower(c) {
            colnum |= bit(C::Lower);
        }
        if bit(C::Upper) != 0 && t.isupper(c) {
            colnum |= bit(C::Upper);
        }
        if bit(C::Graph) != 0 && t.isgraph(c) {
            colnum |= bit(C::Graph);
        }
        colnum
    }
}

/// A compiled regular expression (`struct guts`, with `re_nsub` and `re_info`).
#[derive(Clone, Debug)]
pub struct Guts {
    /// The flags it was compiled under.
    pub cflags: i32,
    /// What the compiler noted of it (`REG_U*`).
    pub info: i64,
    /// Its capturing subexpressions.
    pub nsub: usize,
    /// The tree, by id: `subres[0]` unused, the root at 1.
    pub subres: Vec<Subre>,
    /// The number of nodes of the tree, plus one.
    pub ntree: usize,
    /// The NFA for the fast preliminary search.
    pub search: Cnfa,
    pub cmap: ColorMap,
    /// The lookaround constraints, by number: `lacons[0]` unused. Each holds its `latype` and its
    /// `cnfa`.
    pub lacons: Vec<Subre>,
    /// The character classes and case mapping it was compiled under: back references compare
    /// under them where `cflags` holds `REG_ICASE`.
    pub ctype: Ctype,
}

impl Guts {
    pub fn tree(&self) -> &Subre {
        &self.subres[1]
    }

    /// Whether `len` characters at `x` and at `y` are the same, as a back reference compares them:
    /// in lower case where the expression ignores case (`casecmp`), else as they are (`cmp`).
    pub fn compare(&self, x: &[Chr], y: &[Chr], len: usize) -> bool {
        if self.cflags & REG_ICASE != 0 {
            x[..len]
                .iter()
                .zip(&y[..len])
                .all(|(&a, &b)| a == b || self.ctype.tolower(a) == self.ctype.tolower(b))
        } else {
            x[..len] == y[..len]
        }
    }
}

/// Where a subexpression matched: its start and its end, in characters from the string's start,
/// or -1 and -1 where it matched nothing (`regmatch_t`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct RegMatch {
    pub rm_so: i64,
    pub rm_eo: i64,
}

impl RegMatch {
    pub const UNSET: RegMatch = RegMatch {
        rm_so: -1,
        rm_eo: -1,
    };
}
