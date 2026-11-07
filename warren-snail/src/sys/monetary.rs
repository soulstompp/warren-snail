//! glibc's locale data, as `nl_langinfo_l` gives it for a locale of every category: what
//! PostgreSQL's `pg_localeconv_r` reads an lc_monetary's conventions from.

use std::ffi::{CStr, CString};

/// The bytes of each item a locale gives, in turn; none where glibc has no locale of that name.
pub(crate) fn langinfo(locale: &CString, items: &[libc::nl_item]) -> Option<Vec<Vec<u8>>> {
    // SAFETY: the name is a C string; a locale made here is freed here.
    let loc =
        unsafe { libc::newlocale(libc::LC_ALL_MASK, locale.as_ptr(), std::ptr::null_mut()) };
    if loc.is_null() {
        return None;
    }
    let out = items
        .iter()
        .map(|&i| {
            // SAFETY: nl_langinfo_l returns a string glibc holds as long as the locale lives, and
            // it is copied before the locale is freed.
            unsafe { CStr::from_ptr(libc::nl_langinfo_l(i, loc)) }
                .to_bytes()
                .to_vec()
        })
        .collect();
    // SAFETY: the locale was made above and is used no more.
    unsafe { libc::freelocale(loc) };
    Some(out)
}
