//! The compiler (regcomp.c).

use super::guts::{Chr, Guts, RegError};
use super::locale::Ctype;

pub(super) fn compile(_pattern: &[Chr], _cflags: i32, _ctype: Ctype) -> Result<Guts, RegError> {
    todo!("the compiler")
}
