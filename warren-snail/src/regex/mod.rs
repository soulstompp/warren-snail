//! Regular expressions, by PostgreSQL 19's own engine (Henry Spencer's, `src/backend/regex/`),
//! ported: a pattern of characters compiled under its flags and its collation's classes, then a
//! string searched for it, each subexpression's match given back as PostgreSQL's gives it.
//!
//! The compiler is `comp.rs` (regcomp.c) with what it includes: the lexer (`lex.rs`), the colors
//! (`color.rs`), the NFA (`nfa.rs`), the character vectors (`cvec.rs`) and the classes and
//! collating elements (`classes.rs`, regc_locale.c). The executor is `exec.rs` (regexec.c) with its
//! lazy DFA (`dfa.rs`). The collation's classes and case are `locale.rs` (regc_pg_locale.c).

mod classes;
mod color;
mod comp;
mod cvec;
mod dfa;
mod exec;
pub(crate) mod guts;
mod lex;
mod locale;
mod nfa;

pub use guts::{Chr, RegError, RegMatch};
pub use locale::Ctype;

/// The flags a pattern is compiled under and a string searched under, and what the compiler notes
/// of a pattern, as regex.h names them.
pub mod flags {
    pub use super::guts::{
        REG_ADVANCED, REG_ADVF, REG_BASIC, REG_BOSONLY, REG_EXPANDED, REG_EXPECT, REG_EXTENDED,
        REG_ICASE, REG_NEWLINE, REG_NLANCH, REG_NLSTOP, REG_NOSPEC, REG_NOSUB, REG_NOTBOL,
        REG_NOTEOL, REG_QUOTE, REG_SMALL, REG_UBACKREF, REG_UBBS, REG_UBOUNDS, REG_UBRACES,
        REG_UBSALNUM, REG_UEMPTYMATCH, REG_UIMPOSSIBLE, REG_ULOCALE, REG_ULOOKAROUND, REG_UNONPOSIX,
        REG_UPBOTCH, REG_USHORTEST, REG_UUNPORT, REG_UUNSPEC,
    };
}

/// A compiled regular expression.
#[derive(Clone, Debug)]
pub struct Regex {
    guts: guts::Guts,
}

impl Regex {
    /// A pattern compiled under the flags (`REG_ADVANCED`, `REG_ICASE` and the rest) and the
    /// collation's classes (`pg_regcomp`).
    pub fn compile(pattern: &[Chr], cflags: i32, ctype: Ctype) -> Result<Regex, RegError> {
        comp::compile(pattern, cflags, ctype).map(|guts| Regex { guts })
    }

    /// Its capturing subexpressions.
    pub fn nsub(&self) -> usize {
        self.guts.nsub
    }

    /// What the compiler noted of it (`REG_U*`).
    pub fn info(&self) -> i64 {
        self.guts.info
    }

    /// The string searched from `search_start` (`pg_regexec`): the whole match and the first
    /// `nmatch - 1` subexpressions' where it matches, None where it does not.
    pub fn exec(
        &self,
        string: &[Chr],
        search_start: usize,
        nmatch: usize,
        eflags: i32,
    ) -> Result<Option<Vec<RegMatch>>, RegError> {
        exec::exec(&self.guts, string, search_start, nmatch, eflags, None)
    }

    /// As `exec`, and `REG_EXPECT`'s `rm_extend` written into `details`.
    pub fn exec_details(
        &self,
        string: &[Chr],
        search_start: usize,
        nmatch: usize,
        eflags: i32,
        details: &mut RegMatch,
    ) -> Result<Option<Vec<RegMatch>>, RegError> {
        exec::exec(
            &self.guts,
            string,
            search_start,
            nmatch,
            eflags,
            Some(details),
        )
    }
}
