//! Every call the snail makes into a C library, each behind a safe function: the machine's libm,
//! glibc's character classes and case under C.UTF-8 and its monetary data, and, under the feature
//! `collations`, ICU's and glibc's collation. They are called as PostgreSQL calls them, so that what
//! they answer is what PostgreSQL answers on the same machine. No other module holds unsafe code.

#[cfg(feature = "collations")]
pub(crate) mod collate;
pub(crate) mod ctype;
pub(crate) mod libm;
#[cfg(feature = "collations")]
pub(crate) mod monetary;
