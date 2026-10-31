//! The lexer (regc_lex.c): the pattern read token by token, in the context of a BRE, an ERE or
//! ARE, a literal string, a bound or a bracket, with the leading prefixes and embedded options.

use super::comp::*;
use super::guts::*;

// The lexical contexts.
pub(super) const L_ERE: i32 = 1;
pub(super) const L_BRE: i32 = 2;
pub(super) const L_Q: i32 = 3;
pub(super) const L_EBND: i32 = 4;
pub(super) const L_BBND: i32 = 5;
pub(super) const L_BRACK: i32 = 6;
pub(super) const L_CEL: i32 = 7;
pub(super) const L_ECL: i32 = 8;
pub(super) const L_CCL: i32 = 9;

/// A character literal as a chr (`CHR`).
const fn chr(c: u8) -> Chr {
    c as Chr
}

/// The chr for a newline (`newline`).
pub(super) fn newline() -> Chr {
    chr(b'\n')
}

impl Vars {
    /// Whether the lexer is at the pattern's end (`ATEOS`).
    fn ateos(&self) -> bool {
        self.now >= self.stop
    }

    /// Whether `n` characters are left (`HAVE`).
    fn have(&self, n: usize) -> bool {
        self.stop - self.now >= n
    }

    /// Whether the next character is `c` (`NEXT1`).
    fn next1(&self, c: u8) -> bool {
        !self.ateos() && self.pattern[self.now] == chr(c)
    }

    /// Whether the next two characters are `a` and `b` (`NEXT2`).
    fn next2(&self, a: u8, b: u8) -> bool {
        self.have(2) && self.pattern[self.now] == chr(a) && self.pattern[self.now + 1] == chr(b)
    }

    /// Whether the next three characters are `a`, `b` and `c` (`NEXT3`).
    fn next3(&self, a: u8, b: u8, c: u8) -> bool {
        self.have(3)
            && self.pattern[self.now] == chr(a)
            && self.pattern[self.now + 1] == chr(b)
            && self.pattern[self.now + 2] == chr(c)
    }

    /// The character `k` places on, which is there.
    fn at(&self, k: usize) -> Chr {
        self.pattern[self.now + k]
    }

    /// The next token's kind set, and success (`RET`).
    fn ret(&mut self, c: i32) -> i32 {
        self.nexttype = c;
        1
    }

    /// The next token's kind and value set, and success (`RETV`).
    fn retv(&mut self, c: i32, n: Chr) -> i32 {
        self.nexttype = c;
        self.nextvalue = n;
        1
    }

    /// An error recorded, and failure (`FAILW`).
    fn failw(&mut self, e: i32) -> i32 {
        self.verr(e);
        0
    }

    /// Whether the last token was of a kind (`LASTTYPE`).
    fn lasttype_is(&self, t: i32) -> bool {
        self.lasttype == t
    }

    /// The lexer set up, the leading options read, and the first token taken (`lexstart`).
    pub(super) fn lexstart(&mut self) {
        // may turn on new kinds of syntax
        self.prefixes();
        if self.iserr() {
            return;
        }

        if (self.cflags & REG_QUOTE) != 0 {
            self.lexcon = L_Q;
        } else if (self.cflags & REG_EXTENDED) != 0 {
            self.lexcon = L_ERE;
        } else {
            self.lexcon = L_BRE;
        }

        // at the start
        self.nexttype = EMPTY;
        self.next();
    }

    /// The special prefixes: `***?`, `***=`, `***:`, and an ARE's embedded options (`prefixes`).
    fn prefixes(&mut self) {
        // a literal string has none of this
        if (self.cflags & REG_QUOTE) != 0 {
            return;
        }

        // a leading "***"
        if self.have(4) && self.next3(b'*', b'*', b'*') {
            let c = self.at(3);
            if c == chr(b'?') {
                // "***?" is an error, whose message shows the version
                self.verr(REG_BADPAT);
                return;
            } else if c == chr(b'=') {
                // "***=" makes the rest a literal string
                self.note(REG_UNONPOSIX);
                self.cflags |= REG_QUOTE;
                self.cflags &= !(REG_ADVANCED | REG_EXPANDED | REG_NEWLINE);
                self.now += 4;
                // and no more prefixes can follow
                return;
            } else if c == chr(b':') {
                // "***:" makes it an ARE
                self.note(REG_UNONPOSIX);
                self.cflags |= REG_ADVANCED;
                self.now += 4;
            } else {
                // any other "***" is an error
                self.verr(REG_BADRPT);
                return;
            }
        }

        // BREs and EREs have no embedded options
        if (self.cflags & REG_ADVANCED) != REG_ADVANCED {
            return;
        }

        // an ARE's embedded options
        if self.have(3) && self.next2(b'(', b'?') && self.ctype.isalpha(self.at(2)) {
            self.note(REG_UNONPOSIX);
            self.now += 2;
            while !self.ateos() && self.ctype.isalpha(self.at(0)) {
                let c = self.at(0);
                match u8::try_from(c).unwrap_or(0) {
                    // BREs
                    b'b' => self.cflags &= !(REG_ADVANCED | REG_QUOTE),
                    // case matters
                    b'c' => self.cflags &= !REG_ICASE,
                    // plain EREs
                    b'e' => {
                        self.cflags |= REG_EXTENDED;
                        self.cflags &= !(REG_ADVF | REG_QUOTE);
                    }
                    // case is ignored
                    b'i' => self.cflags |= REG_ICASE,
                    // \n affects ^ $ . [^
                    b'm' | b'n' => self.cflags |= REG_NEWLINE,
                    // \n affects . [^
                    b'p' => {
                        self.cflags |= REG_NLSTOP;
                        self.cflags &= !REG_NLANCH;
                    }
                    // a literal string
                    b'q' => {
                        self.cflags |= REG_QUOTE;
                        self.cflags &= !REG_ADVANCED;
                    }
                    // one line, \n ordinary
                    b's' => self.cflags &= !REG_NEWLINE,
                    // tight syntax
                    b't' => self.cflags &= !REG_EXPANDED,
                    // \n affects ^ $ only
                    b'w' => {
                        self.cflags &= !REG_NLSTOP;
                        self.cflags |= REG_NLANCH;
                    }
                    // expanded syntax
                    b'x' => self.cflags |= REG_EXPANDED,
                    _ => {
                        self.verr(REG_BADOPT);
                        return;
                    }
                }
                self.now += 1;
            }
            if !self.next1(b')') {
                self.verr(REG_BADOPT);
                return;
            }
            self.now += 1;
            if (self.cflags & REG_QUOTE) != 0 {
                self.cflags &= !(REG_EXPANDED | REG_NEWLINE);
            }
        }
    }

    /// The next token: 1, or 0 where it fails (`next`).
    pub(super) fn next(&mut self) -> i32 {
        loop {
            // after an error, failures only; the error has made the token EOS
            if self.iserr() {
                return 0;
            }

            // the last token's kind
            self.lasttype = self.nexttype;

            // REG_BOSONLY: at the start, as \A
            if self.nexttype == EMPTY && (self.cflags & REG_BOSONLY) != 0 {
                return self.retv(SBEGIN, 0);
            }

            // white space and comments skipped where they may be
            if (self.cflags & REG_EXPANDED) != 0
                && matches!(self.lexcon, L_ERE | L_BRE | L_EBND | L_BBND)
            {
                self.skip();
            }

            // the pattern's end, by context
            if self.ateos() {
                return match self.lexcon {
                    L_ERE | L_BRE | L_Q => self.ret(EOS),
                    L_EBND | L_BBND => self.failw(REG_EBRACE),
                    _ => self.failw(REG_EBRACK),
                };
            }

            // a character
            let mut c = self.pattern[self.now];
            self.now += 1;

            // the easy contexts; EREs below
            match self.lexcon {
                L_BRE => return self.brenext(c),
                L_ERE => {}
                L_Q => return self.retv(PLAIN, c),
                L_BBND | L_EBND => {
                    return match u8::try_from(c).unwrap_or(0) {
                        d @ b'0'..=b'9' => self.retv(DIGIT, (d - b'0') as Chr),
                        b',' => self.ret(b',' as i32),
                        b'}' => {
                            // an ERE's bound ends with }
                            if self.lexcon == L_EBND {
                                self.lexcon = L_ERE;
                                if (self.cflags & REG_ADVF) != 0 && self.next1(b'?') {
                                    self.now += 1;
                                    self.note(REG_UNONPOSIX);
                                    return self.retv(b'}' as i32, 0);
                                }
                                self.retv(b'}' as i32, 1)
                            } else {
                                self.failw(REG_BADBR)
                            }
                        }
                        b'\\' => {
                            // a BRE's bound ends with \}
                            if self.lexcon == L_BBND && self.next1(b'}') {
                                self.now += 1;
                                self.lexcon = L_BRE;
                                self.retv(b'}' as i32, 1)
                            } else {
                                self.failw(REG_BADBR)
                            }
                        }
                        _ => self.failw(REG_BADBR),
                    };
                }
                L_BRACK => {
                    return match u8::try_from(c).unwrap_or(0) {
                        b']' => {
                            if self.lasttype_is(b'[' as i32) {
                                self.retv(PLAIN, c)
                            } else {
                                self.lexcon = if (self.cflags & REG_EXTENDED) != 0 {
                                    L_ERE
                                } else {
                                    L_BRE
                                };
                                self.ret(b']' as i32)
                            }
                        }
                        b'\\' => {
                            self.note(REG_UBBS);
                            if (self.cflags & REG_ADVF) == 0 {
                                return self.retv(PLAIN, c);
                            }
                            self.note(REG_UNONPOSIX);
                            if self.ateos() {
                                return self.failw(REG_EESCAPE);
                            }
                            if self.lexescape() == 0 {
                                return 0;
                            }
                            match self.nexttype {
                                // not every escape is allowed here
                                PLAIN | CCLASSS | CCLASSC => 1,
                                _ => self.failw(REG_EESCAPE),
                            }
                        }
                        b'-' => {
                            if self.lasttype_is(b'[' as i32) || self.next1(b']') {
                                self.retv(PLAIN, c)
                            } else {
                                self.retv(RANGE, c)
                            }
                        }
                        b'[' => {
                            if self.ateos() {
                                return self.failw(REG_EBRACK);
                            }
                            let d = self.pattern[self.now];
                            self.now += 1;
                            match u8::try_from(d).unwrap_or(0) {
                                b'.' => {
                                    self.lexcon = L_CEL;
                                    // locale-specific or not
                                    self.ret(COLLEL)
                                }
                                b'=' => {
                                    self.lexcon = L_ECL;
                                    self.note(REG_ULOCALE);
                                    self.ret(ECLASS)
                                }
                                b':' => {
                                    self.lexcon = L_CCL;
                                    self.note(REG_ULOCALE);
                                    self.ret(CCLASS)
                                }
                                _ => {
                                    self.now -= 1;
                                    self.retv(PLAIN, c)
                                }
                            }
                        }
                        _ => self.retv(PLAIN, c),
                    };
                }
                L_CEL => {
                    // collating elements are easy
                    if c == chr(b'.') && self.next1(b']') {
                        self.now += 1;
                        self.lexcon = L_BRACK;
                        return self.retv(END, chr(b'.'));
                    }
                    return self.retv(PLAIN, c);
                }
                L_ECL => {
                    // and equivalence classes
                    if c == chr(b'=') && self.next1(b']') {
                        self.now += 1;
                        self.lexcon = L_BRACK;
                        return self.retv(END, chr(b'='));
                    }
                    return self.retv(PLAIN, c);
                }
                L_CCL => {
                    // and character classes
                    if c == chr(b':') && self.next1(b']') {
                        self.now += 1;
                        self.lexcon = L_BRACK;
                        return self.retv(END, chr(b':'));
                    }
                    return self.retv(PLAIN, c);
                }
                _ => {}
            }

            // EREs and AREs, but for backslashes
            match u8::try_from(c).unwrap_or(0) {
                b'|' => return self.ret(b'|' as i32),
                q @ (b'*' | b'+' | b'?') => {
                    if (self.cflags & REG_ADVF) != 0 && self.next1(b'?') {
                        self.now += 1;
                        self.note(REG_UNONPOSIX);
                        return self.retv(q as i32, 0);
                    }
                    return self.retv(q as i32, 1);
                }
                b'{' => {
                    // a bound's start, or a plain character
                    if (self.cflags & REG_EXPANDED) != 0 {
                        self.skip();
                    }
                    if self.ateos() || !self.ctype.isdigit(self.at(0)) {
                        self.note(REG_UBRACES);
                        self.note(REG_UUNSPEC);
                        return self.retv(PLAIN, c);
                    }
                    self.note(REG_UBOUNDS);
                    self.lexcon = L_EBND;
                    return self.ret(b'{' as i32);
                }
                b'(' => {
                    // a parenthesis, or an ARE's extension
                    if (self.cflags & REG_ADVF) != 0 && self.next1(b'?') {
                        self.note(REG_UNONPOSIX);
                        self.now += 1;
                        if self.ateos() {
                            return self.failw(REG_BADRPT);
                        }
                        let d = self.pattern[self.now];
                        self.now += 1;
                        match u8::try_from(d).unwrap_or(0) {
                            // non-capturing
                            b':' => return self.retv(b'(' as i32, 0),
                            b'#' => {
                                // a comment
                                while !self.ateos() && self.at(0) != chr(b')') {
                                    self.now += 1;
                                }
                                if !self.ateos() {
                                    self.now += 1;
                                }
                                continue;
                            }
                            b'=' => {
                                self.note(REG_ULOOKAROUND);
                                return self.retv(LACON, LATYPE_AHEAD_POS as Chr);
                            }
                            b'!' => {
                                self.note(REG_ULOOKAROUND);
                                return self.retv(LACON, LATYPE_AHEAD_NEG as Chr);
                            }
                            b'<' => {
                                if self.ateos() {
                                    return self.failw(REG_BADRPT);
                                }
                                let e = self.pattern[self.now];
                                self.now += 1;
                                return match u8::try_from(e).unwrap_or(0) {
                                    b'=' => {
                                        self.note(REG_ULOOKAROUND);
                                        self.retv(LACON, LATYPE_BEHIND_POS as Chr)
                                    }
                                    b'!' => {
                                        self.note(REG_ULOOKAROUND);
                                        self.retv(LACON, LATYPE_BEHIND_NEG as Chr)
                                    }
                                    _ => self.failw(REG_BADRPT),
                                };
                            }
                            _ => return self.failw(REG_BADRPT),
                        }
                    }
                    return self.retv(b'(' as i32, 1);
                }
                b')' => {
                    if self.lasttype_is(b'(' as i32) {
                        self.note(REG_UUNSPEC);
                    }
                    return self.retv(b')' as i32, c);
                }
                b'[' => {
                    // easy, but for [[:<:]] and [[:>:]]
                    if let Some(t) = self.word_bracket() {
                        return self.ret(t);
                    }
                    self.lexcon = L_BRACK;
                    if self.next1(b'^') {
                        self.now += 1;
                        return self.retv(b'[' as i32, 0);
                    }
                    return self.retv(b'[' as i32, 1);
                }
                b'.' => return self.ret(b'.' as i32),
                b'^' => return self.ret(b'^' as i32),
                b'$' => return self.ret(b'$' as i32),
                b'\\' => {
                    // backslashes below
                    if self.ateos() {
                        return self.failw(REG_EESCAPE);
                    }
                }
                // an ordinary character
                _ => return self.retv(PLAIN, c),
            }

            // an ERE's or ARE's backslash, eaten already
            if (self.cflags & REG_ADVF) == 0 {
                // only AREs have escapes of note
                if self.ctype.isalnum(self.at(0)) {
                    self.note(REG_UBSALNUM);
                    self.note(REG_UUNSPEC);
                }
                c = self.pattern[self.now];
                self.now += 1;
                return self.retv(PLAIN, c);
            }
            return self.lexescape();
        }
    }

    /// `[[:<:]]` or `[[:>:]]` after a `[`: the token `<` or `>`, its characters taken.
    fn word_bracket(&mut self) -> Option<i32> {
        if self.have(6)
            && self.at(0) == chr(b'[')
            && self.at(1) == chr(b':')
            && (self.at(2) == chr(b'<') || self.at(2) == chr(b'>'))
            && self.at(3) == chr(b':')
            && self.at(4) == chr(b']')
            && self.at(5) == chr(b']')
        {
            let c = self.at(2);
            self.now += 6;
            self.note(REG_UNONPOSIX);
            return Some(if c == chr(b'<') { b'<' } else { b'>' } as i32);
        }
        None
    }

    /// An ARE's backslash escape, the backslash eaten, alone or in a bracket; the caller refuses
    /// the kinds a bracket does not allow (`lexescape`).
    fn lexescape(&mut self) -> i32 {
        const ALERT: [Chr; 5] = [chr(b'a'), chr(b'l'), chr(b'e'), chr(b'r'), chr(b't')];
        const ESC: [Chr; 3] = [chr(b'E'), chr(b'S'), chr(b'C')];

        let mut c = self.pattern[self.now];
        self.now += 1;

        // not ASCII's alphanumerics: a plain character
        if !(chr(b'a')..=chr(b'z')).contains(&c)
            && !(chr(b'A')..=chr(b'Z')).contains(&c)
            && !(chr(b'0')..=chr(b'9')).contains(&c)
        {
            return self.retv(PLAIN, c);
        }

        self.note(REG_UNONPOSIX);
        match c as u8 {
            b'a' => {
                let n = self.chrnamed(&ALERT, chr(0o7));
                self.retv(PLAIN, n)
            }
            b'A' => self.retv(SBEGIN, 0),
            b'b' => self.retv(PLAIN, chr(0o10)),
            b'B' => self.retv(PLAIN, chr(b'\\')),
            b'c' => {
                self.note(REG_UUNPORT);
                if self.ateos() {
                    return self.failw(REG_EESCAPE);
                }
                let d = self.pattern[self.now];
                self.now += 1;
                self.retv(PLAIN, d & 0o37)
            }
            b'd' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSS, CharClass::Digit as Chr)
            }
            b'D' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSC, CharClass::Digit as Chr)
            }
            b'e' => {
                self.note(REG_UUNPORT);
                let n = self.chrnamed(&ESC, chr(0o33));
                self.retv(PLAIN, n)
            }
            b'f' => self.retv(PLAIN, chr(0o14)),
            b'm' => self.ret(b'<' as i32),
            b'M' => self.ret(b'>' as i32),
            b'n' => self.retv(PLAIN, chr(b'\n')),
            b'r' => self.retv(PLAIN, chr(b'\r')),
            b's' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSS, CharClass::Space as Chr)
            }
            b'S' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSC, CharClass::Space as Chr)
            }
            b't' => self.retv(PLAIN, chr(b'\t')),
            b'u' => {
                c = self.lexdigits(16, 4, 4);
                if self.iserr() || c > CHR_MAX {
                    return self.failw(REG_EESCAPE);
                }
                self.retv(PLAIN, c)
            }
            b'U' => {
                c = self.lexdigits(16, 8, 8);
                if self.iserr() || c > CHR_MAX {
                    return self.failw(REG_EESCAPE);
                }
                self.retv(PLAIN, c)
            }
            b'v' => self.retv(PLAIN, chr(0o13)),
            b'w' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSS, CharClass::Word as Chr)
            }
            b'W' => {
                self.note(REG_ULOCALE);
                self.retv(CCLASSC, CharClass::Word as Chr)
            }
            b'x' => {
                self.note(REG_UUNPORT);
                // REs longer than 255 are outside the specification
                c = self.lexdigits(16, 1, 255);
                if self.iserr() || c > CHR_MAX {
                    return self.failw(REG_EESCAPE);
                }
                self.retv(PLAIN, c)
            }
            b'y' => {
                self.note(REG_ULOCALE);
                self.retv(WBDRY, 0)
            }
            b'Y' => {
                self.note(REG_ULOCALE);
                self.retv(NWBDRY, 0)
            }
            b'Z' => self.retv(SEND, 0),
            b'1'..=b'9' => {
                let save = self.now;
                // the first digit put back
                self.now -= 1;
                c = self.lexdigits(10, 1, 255);
                if self.iserr() {
                    return self.failw(REG_EESCAPE);
                }
                // one digit, or a number of a subexpression seen
                if self.now == save || ((c as i32) > 0 && (c as i32) <= self.nsubexp) {
                    self.note(REG_UBACKREF);
                    return self.retv(BACKREF, c);
                }
                // not a back reference after all, but an octal number
                self.now = save;
                self.octal()
            }
            b'0' => self.octal(),
            // ASCII's other letters are kept for the future
            _ => self.failw(REG_EESCAPE),
        }
    }

    /// An escape's octal number, its first digit eaten already (`lexescape`'s `\0` case).
    fn octal(&mut self) -> i32 {
        self.note(REG_UUNPORT);
        // the first digit put back
        self.now -= 1;
        let mut c = self.lexdigits(8, 1, 3);
        if self.iserr() {
            return self.failw(REG_EESCAPE);
        }
        if c > 0xff {
            // past a byte: one digit too many
            self.now -= 1;
            c >>= 3;
        }
        self.retv(PLAIN, c)
    }

    /// Digits of a base read, `minlen` to `maxlen` of them: their value, overflow wrapping
    /// (`lexdigits`).
    fn lexdigits(&mut self, base: u32, minlen: usize, maxlen: usize) -> Chr {
        let mut n: u32 = 0;
        let mut len = 0;
        while len < maxlen && !self.ateos() {
            let c = self.pattern[self.now];
            self.now += 1;
            let mut d: i32 = match u8::try_from(c).unwrap_or(0) {
                x @ b'0'..=b'9' => (x - b'0') as i32,
                b'a' | b'A' => 10,
                b'b' | b'B' => 11,
                b'c' | b'C' => 12,
                b'd' | b'D' => 13,
                b'e' | b'E' => 14,
                b'f' | b'F' => 15,
                _ => {
                    // not a digit at all
                    self.now -= 1;
                    -1
                }
            };
            if d >= base as i32 {
                // not a digit of the base
                self.now -= 1;
                d = -1;
            }
            if d < 0 {
                break;
            }
            n = n.wrapping_mul(base).wrapping_add(d as u32);
            len += 1;
        }
        if len < minlen {
            self.verr(REG_EESCAPE);
        }
        n
    }

    /// The next token of a BRE, its first character read (`brenext`).
    fn brenext(&mut self, c: Chr) -> i32 {
        match u8::try_from(c).unwrap_or(0) {
            b'*' => {
                if self.lasttype_is(EMPTY)
                    || self.lasttype_is(b'(' as i32)
                    || self.lasttype_is(b'^' as i32)
                {
                    return self.retv(PLAIN, c);
                }
                return self.retv(b'*' as i32, 1);
            }
            b'[' => {
                if let Some(t) = self.word_bracket() {
                    return self.ret(t);
                }
                self.lexcon = L_BRACK;
                if self.next1(b'^') {
                    self.now += 1;
                    return self.retv(b'[' as i32, 0);
                }
                return self.retv(b'[' as i32, 1);
            }
            b'.' => return self.ret(b'.' as i32),
            b'^' => {
                if self.lasttype_is(EMPTY) {
                    return self.ret(b'^' as i32);
                }
                if self.lasttype_is(b'(' as i32) {
                    self.note(REG_UUNSPEC);
                    return self.ret(b'^' as i32);
                }
                return self.retv(PLAIN, c);
            }
            b'$' => {
                if (self.cflags & REG_EXPANDED) != 0 {
                    self.skip();
                }
                if self.ateos() {
                    return self.ret(b'$' as i32);
                }
                if self.next2(b'\\', b')') {
                    self.note(REG_UUNSPEC);
                    return self.ret(b'$' as i32);
                }
                return self.retv(PLAIN, c);
            }
            b'\\' => {}
            _ => return self.retv(PLAIN, c),
        }

        // a backslash
        if self.ateos() {
            return self.failw(REG_EESCAPE);
        }

        let c = self.pattern[self.now];
        self.now += 1;
        match u8::try_from(c).unwrap_or(0) {
            b'{' => {
                self.lexcon = L_BBND;
                self.note(REG_UBOUNDS);
                self.ret(b'{' as i32)
            }
            b'(' => self.retv(b'(' as i32, 1),
            b')' => self.retv(b')' as i32, c),
            b'<' => {
                self.note(REG_UNONPOSIX);
                self.ret(b'<' as i32)
            }
            b'>' => {
                self.note(REG_UNONPOSIX);
                self.ret(b'>' as i32)
            }
            d @ b'1'..=b'9' => {
                self.note(REG_UBACKREF);
                self.retv(BACKREF, (d - b'0') as Chr)
            }
            _ => {
                if self.ctype.isalnum(c) {
                    self.note(REG_UBSALNUM);
                    self.note(REG_UUNSPEC);
                }
                self.retv(PLAIN, c)
            }
        }
    }

    /// White space and comments of the expanded syntax skipped (`skip`).
    fn skip(&mut self) {
        let start = self.now;
        loop {
            while !self.ateos() && self.ctype.isspace(self.at(0)) {
                self.now += 1;
            }
            if self.ateos() || self.at(0) != chr(b'#') {
                break;
            }
            while !self.ateos() && self.at(0) != chr(b'\n') {
                self.now += 1;
            }
            // the newline is left to the white space's loop
        }
        if self.now != start {
            self.note(REG_UNONPOSIX);
        }
    }

    /// The character a name stands for, or `lastresort` where the name finds none (`chrnamed`).
    fn chrnamed(&mut self, name: &[Chr], lastresort: Chr) -> Chr {
        let errsave = self.err;
        self.err = 0;
        let c = self.element(name);
        let e = self.err;
        self.err = errsave;

        if e != 0 {
            return lastresort;
        }

        match self.range(c, c, false) {
            Some(cv) if !cv.chrs.is_empty() => cv.chrs[0],
            _ => lastresort,
        }
    }
}
