//! Prepared statements, as PostgreSQL's PREPARE keeps them for its session: each its name, the types
//! of its parameters and its statement, run by EXECUTE with its parameters' values.

/// A statement PREPARE keeps.
#[derive(Clone, Debug)]
pub(super) struct Prepared {
    /// Its parameters' types as PREPARE names them, a type PREPARE leaves out inferred from where
    /// the parameter stands.
    pub(super) types: Vec<Option<String>>,
    /// The statement, as written.
    pub(super) statement: String,
}
