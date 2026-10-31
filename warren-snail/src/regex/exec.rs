//! The executor (regexec.c).

use super::guts::{Chr, Guts, RegError, RegMatch};

/// `pg_regexec`: the string searched from `search_start`, the first `nmatch` matches given back
/// where it matches (the whole match first), None where it does not; `details`, where given, the
/// `rm_extend` of `REG_EXPECT`.
pub(super) fn exec(
    _g: &Guts,
    _string: &[Chr],
    _search_start: usize,
    _nmatch: usize,
    _eflags: i32,
    _details: Option<&mut RegMatch>,
) -> Result<Option<Vec<RegMatch>>, RegError> {
    todo!("the executor")
}
