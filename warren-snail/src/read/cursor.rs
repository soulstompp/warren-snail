//! Cursors, as PostgreSQL's DECLARE opens them: a question's answer read as its declaration read it,
//! FETCH and MOVE taking rows from where the cursor stands.

/// A cursor DECLARE opened.
pub(super) struct Cursor {
    /// Its number in the session, which tells it from another of its name declared since.
    pub(super) id: u64,
    pub(super) name: String,
    /// Declared WITH HOLD: it outlives the transaction that commits it.
    pub(super) hold: bool,
}
