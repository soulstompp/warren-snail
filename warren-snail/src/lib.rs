#![doc = include_str!("../README.md")]
#![deny(unsafe_code)]

pub mod arrays;
pub mod brin;
pub mod bytes;
pub mod carry;
pub mod catalog;
mod categories;
pub mod collations;
pub mod composed;
pub mod cut;
mod ddl;
pub mod directory;
pub mod encode;
mod error;
pub mod file;
pub mod formatting;
pub mod functions;
pub mod geo;
mod grammar;
#[cfg(feature = "standalone")]
pub mod held;
pub mod inet;
pub mod jsonb;
pub mod jsonpath;
pub mod lists;
pub(crate) mod listing;
pub mod lock;
mod methods;
pub mod monetary;
pub mod numeric;
pub mod oids;
pub(crate) mod pgcatalog;
pub mod places;
pub(crate) mod plane;
pub mod read;
pub mod regex;
pub mod regexp;
pub mod regs;
pub mod room;
pub mod rowtypes;
pub mod scalars;
pub(crate) mod spans;
pub mod strings;
// Every call into a C library, and the only unsafe code the crate holds.
#[allow(unsafe_code)]
pub(crate) mod sys;
pub mod textual;
pub mod times;
pub mod tsearch;
pub mod values;
pub mod write;
pub mod xml;
pub mod zones;

pub use error::{Error, Result};
