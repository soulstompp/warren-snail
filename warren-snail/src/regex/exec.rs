//! The executor (regexec.c).

use super::guts::{Chr, Guts, RegError, RegMatch};

pub(super) fn exec(
    _g: &Guts,
    _string: &[Chr],
    _search_start: usize,
    _nmatch: usize,
    _eflags: i32,
) -> Result<Option<Vec<RegMatch>>, RegError> {
    todo!("the executor")
}
