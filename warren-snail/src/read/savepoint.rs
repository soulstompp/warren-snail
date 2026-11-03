//! A transaction's savepoints, as PostgreSQL's subtransactions: each the transaction as it stood
//! when it was named. ROLLBACK TO puts it back, drops the pages staged since and lets go of the
//! block and row locks taken since; RELEASE folds it into the transaction. A sequence's advance is
//! never given back, and at SERIALIZABLE the transaction's marks stay.

use super::change::{Declared, Rebased};
use super::foreign::Touched;
use super::statement::Levels;
use super::Snail;
use crate::catalog::Catalog;
use crate::file::Root;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// The transaction as it stood when a savepoint was named.
pub(super) struct Savepoint {
    name: String,
    rebased: BTreeMap<(usize, usize), Vec<Rebased>>,
    staged: u64,
    root: Root,
    catalog: Catalog,
    declared: Option<Declared>,
    wrote: bool,
    locks: BTreeSet<u64>,
    row_locks: BTreeMap<u64, bool>,
    unnamed: BTreeSet<usize>,
    touched: Touched,
    pending: Touched,
    modes: BTreeMap<(usize, usize), bool>,
    own: BTreeMap<String, super::beside::OwnRows>,
    first_bytes: BTreeMap<u64, u64>,
    marking: Option<Option<String>>,
    levels: Levels,
    /// What the session had set, its cursors and its settings, put back with it.
    pub(super) session: super::session::Kept,
}

/// The words PostgreSQL refuses a savepoint's statement in outside a transaction.
fn outside(what: &str) -> Error {
    Error::new(format!("{what} can only be used in transaction blocks"))
}

impl Snail {
    /// SAVEPOINT: the transaction as it stands, kept under `name`.
    pub(super) fn savepoint(&mut self, name: &str) -> Result<()> {
        let wrote = match &self.begun {
            Some((_, wrote)) => *wrote,
            None => return Err(outside("SAVEPOINT")),
        };
        let kept = Savepoint {
            name: name.to_string(),
            rebased: self.rebased.clone(),
            staged: self.reader.staged_mark(),
            root: self.root.clone(),
            catalog: self.catalog.clone(),
            declared: self.declared.clone(),
            wrote,
            locks: self.locks.clone(),
            row_locks: self.row_locks.clone(),
            unnamed: self.unnamed.clone(),
            touched: self.touched.clone(),
            pending: self.pending.clone(),
            modes: self.modes.clone(),
            own: self.own.clone(),
            first_bytes: self.first_bytes.clone(),
            marking: self.marking.clone(),
            levels: self.levels,
            session: self.session.kept(),
        };
        self.savepoints.push(kept);
        Ok(())
    }

    /// The newest savepoint of that name, or PostgreSQL's words where none is.
    fn savepoint_named(&self, name: &str, what: &str) -> Result<usize> {
        if self.begun.is_none() {
            return Err(outside(what));
        }
        self.savepoints
            .iter()
            .rposition(|s| s.name == name)
            .ok_or_else(|| Error::new(format!("savepoint \"{name}\" does not exist")))
    }

    /// ROLLBACK TO SAVEPOINT: the transaction put back as it stood at the newest savepoint of that
    /// name, which stays; the savepoints named after it are gone. A transaction one of whose
    /// statements failed goes on from there.
    pub(super) fn rollback_to(&mut self, name: &str) -> Result<()> {
        let at = self.savepoint_named(name, "ROLLBACK TO SAVEPOINT")?;
        self.savepoints.truncate(at + 1);
        self.back_to_savepoint(at)?;
        self.aborted = false;
        Ok(())
    }

    /// RELEASE SAVEPOINT: the newest savepoint of that name gone, and every one named after it,
    /// what was done since kept as the transaction's.
    pub(super) fn release_savepoint(&mut self, name: &str) -> Result<()> {
        let at = self.savepoint_named(name, "RELEASE SAVEPOINT")?;
        self.savepoints.truncate(at);
        Ok(())
    }

    /// Whether the transaction under way has a savepoint, as a subtransaction PostgreSQL runs.
    pub(super) fn in_savepoint(&self) -> bool {
        self.begun.is_some() && !self.savepoints.is_empty()
    }

    /// A statement failed: in a savepoint, what it and the statements since the newest savepoint
    /// did is let go at once, as PostgreSQL aborts a subtransaction, and the transaction takes only
    /// ROLLBACK TO or its end; otherwise everything the statement or the transaction staged is
    /// dropped, and a transaction takes only its end.
    pub(super) fn failed(&mut self) -> Result<()> {
        if self.in_savepoint() {
            self.back_to_savepoint(self.savepoints.len() - 1)?;
            self.aborted = true;
            return Ok(());
        }
        // What it dropped named the root it staged; the transaction stands where it began.
        if let Some((began, _)) = self.begun.as_ref() {
            self.root = began.clone();
        }
        self.abandon()?;
        self.aborted = self.begun.is_some();
        Ok(())
    }

    /// The transaction as it stood at the `at`th savepoint: its staged blocks and the root it reads
    /// with them, the pages staged since dropped, the locks taken since let go, and what its
    /// foreign keys, its catalog, its settings and its cursors had come to.
    fn back_to_savepoint(&mut self, at: usize) -> Result<()> {
        let s = &self.savepoints[at];
        self.rebased = s.rebased.clone();
        self.reader.staged_drop_since(s.staged);
        self.root = s.root.clone();
        self.catalog = s.catalog.clone();
        self.declared = s.declared.clone();
        if let Some((_, wrote)) = self.begun.as_mut() {
            *wrote = s.wrote;
        }
        self.unnamed = s.unnamed.clone();
        self.touched = s.touched.clone();
        self.pending = s.pending.clone();
        self.modes = s.modes.clone();
        self.own = s.own.clone();
        self.first_bytes = s.first_bytes.clone();
        self.marking = s.marking.clone();
        self.levels.back_to(&s.levels);
        let session = s.session.clone();
        let (locks, row_locks) = (s.locks.clone(), s.row_locks.clone());
        self.session.back_to(session);
        // The locks taken since let go; a row's lock taken shared before and alone since held
        // shared again.
        for b in std::mem::replace(&mut self.locks, locks.clone()) {
            if !locks.contains(&b) {
                self.unlock_byte(b)?;
            }
        }
        for (b, shared) in std::mem::replace(&mut self.row_locks, row_locks.clone()) {
            match row_locks.get(&b) {
                None => self.unlock_byte(b)?,
                Some(&was) if was && !shared => {
                    if let Some(w) = self.writer.as_ref() {
                        crate::lock::lock_as(w.file(), b, true, false)?;
                    }
                }
                Some(_) => {}
            }
        }
        self.table_locks_back_to(&self.savepoints[at].session.table_locks.clone())?;
        Ok(())
    }
}
