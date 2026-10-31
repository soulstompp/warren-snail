//! The character vectors (regc_cvec.c): the characters and ranges a bracket, a class or a case
//! names, handed to the colors.

use super::comp::Vars;
use super::guts::Chr;

/// A set of characters: single characters, ranges low to high, and where it is a class whose
/// characters past the ones probed are asked at run time, the class; else -1 (`struct cvec`).
#[derive(Clone, Debug, Default)]
pub(super) struct Cvec {
    pub(super) chrs: Vec<Chr>,
    /// How many characters it has room for.
    pub(super) chrspace: usize,
    pub(super) ranges: Vec<(Chr, Chr)>,
    pub(super) cclasscode: i32,
}

impl Cvec {
    /// An empty vector with room for `nchrs` characters (`newcvec`, `clearcvec`).
    pub(super) fn newcvec(nchrs: usize) -> Cvec {
        Cvec {
            chrs: Vec::new(),
            chrspace: nchrs,
            ranges: Vec::new(),
            cclasscode: -1,
        }
    }

    /// A character added (`addchr`).
    pub(super) fn addchr(&mut self, c: Chr) {
        self.chrs.push(c);
    }

    /// A range added (`addrange`).
    pub(super) fn addrange(&mut self, from: Chr, to: Chr) {
        self.ranges.push((from, to));
    }
}

impl Vars {
    /// The transient vector, empty, with room for at least `nchrs` characters and `nranges`
    /// ranges: the room it has where that is enough, else just that (`getcvec`).
    pub(super) fn getcvec(&mut self, nchrs: usize, nranges: usize) -> Cvec {
        if !(nchrs <= self.cv_chrspace && nranges <= self.cv_rangespace) {
            self.cv_chrspace = nchrs;
            self.cv_rangespace = nranges;
        }
        Cvec::newcvec(self.cv_chrspace)
    }
}
