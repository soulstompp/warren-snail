//! What a writer's session holds between its statements, as PostgreSQL's does: the settings it takes
//! beside those the snail answers under, its prepared statements, its cursors and the tables' locks
//! its transaction holds.

use super::tablelock::Mode;
use crate::catalog::Catalog;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// A statement of the session's own, as the snail reads it.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Said {
    /// COMMIT AND CHAIN, or ROLLBACK AND CHAIN.
    Chain {
        commit: bool,
    },
    Savepoint {
        name: String,
    },
    RollbackTo {
        name: String,
    },
    Release {
        name: String,
    },
    /// LOCK TABLE: the tables by their places in the catalog, the mode, and NOWAIT.
    Lock {
        tables: Vec<usize>,
        mode: Mode,
        nowait: bool,
    },
}

impl Said {
    /// The command's tag, as PostgreSQL's names it.
    pub fn kind(&self) -> &'static str {
        match self {
            Said::Chain { commit: true } => "COMMIT",
            Said::Chain { commit: false } => "ROLLBACK",
            Said::Savepoint { .. } => "SAVEPOINT",
            Said::RollbackTo { .. } => "ROLLBACK",
            Said::Release { .. } => "RELEASE",
            Said::Lock { .. } => "LOCK TABLE",
        }
    }
}

/// A statement of the session's own read off its words, where sqlparser does not read it as
/// PostgreSQL does: none where it is no such statement.
pub(super) fn said(
    catalog: &Catalog,
    _sql: &str,
    tokens: &[sqlparser::tokenizer::Token],
) -> Result<Option<Said>> {
    if let Some(read) = super::tablelock::parse_lock(tokens) {
        let (names, mode, nowait) = read?;
        let tables = names
            .iter()
            .map(|n| {
                catalog
                    .tables
                    .iter()
                    .position(|t| t.name == *n)
                    .ok_or_else(|| Error::new(format!("{n} is not in the snail")))
            })
            .collect::<Result<_>>()?;
        return Ok(Some(Said::Lock {
            tables,
            mode,
            nowait,
        }));
    }
    Ok(None)
}

/// A writer's session.
#[derive(Default)]
pub(super) struct Session {
    pub(super) settings: Settings,
    pub(super) prepared: BTreeMap<String, super::prepared::Prepared>,
    pub(super) cursors: Vec<super::cursor::Cursor>,
    pub(super) table_locks: BTreeSet<(usize, Mode)>,
}

/// What a savepoint keeps of the session: the settings, which cursors were open, and the tables'
/// locks its transaction held.
#[derive(Clone)]
pub(super) struct Kept {
    settings: Settings,
    cursors: Vec<u64>,
    pub(super) table_locks: BTreeSet<(usize, Mode)>,
}

impl Session {
    pub(super) fn kept(&self) -> Kept {
        Kept {
            settings: self.settings.clone(),
            cursors: self.cursors.iter().map(|c| c.id).collect(),
            table_locks: self.table_locks.clone(),
        }
    }

    /// The session as a savepoint kept it: its settings put back, and the cursors declared since
    /// closed. The tables' locks are let go by the snail.
    pub(super) fn back_to(&mut self, kept: Kept) {
        self.settings = kept.settings;
        self.cursors.retain(|c| kept.cursors.contains(&c.id));
    }

    /// The transaction ended: its settings kept where it committed, else put back; its cursors
    /// closed but those declared WITH HOLD where it committed.
    pub(super) fn end(&mut self, committed: bool) {
        self.settings.end(committed);
        self.cursors.retain(|c| committed && c.hold);
    }
}

/// The settings a session has taken beside those the snail answers under, as PostgreSQL nests them:
/// those it would keep after its transaction, and those that stand now, a SET LOCAL's among them.
#[derive(Clone, Default)]
pub(super) struct Settings {
    session: BTreeMap<String, String>,
    now: BTreeMap<String, String>,
    /// Outside a transaction, the settings as they stand: what a transaction puts back.
    committed: BTreeMap<String, String>,
}

impl Settings {
    /// The value a setting stands at, where the session set it.
    pub(super) fn get(&self, name: &str) -> Option<&str> {
        self.now.get(name).map(String::as_str)
    }

    /// A SET: of the session, which outlives its transaction where it commits, or LOCAL, for the
    /// transaction alone; `None` sets the default back.
    pub(super) fn set(&mut self, name: &str, value: Option<String>, local: bool, begun: bool) {
        match &value {
            Some(v) => self.now.insert(name.to_string(), v.clone()),
            None => self.now.remove(name),
        };
        if !local {
            match value {
                Some(v) => self.session.insert(name.to_string(), v),
                None => self.session.remove(name),
            };
        }
        if !begun {
            self.committed = self.session.clone();
        }
    }

    /// RESET ALL: every setting the session took back at its default.
    pub(super) fn reset_all(&mut self, begun: bool) {
        self.now.clear();
        self.session.clear();
        if !begun {
            self.committed.clear();
        }
    }

    /// The settings the session set, by name.
    pub(super) fn all(&self) -> &BTreeMap<String, String> {
        &self.now
    }

    fn end(&mut self, committed: bool) {
        if committed {
            self.committed = self.session.clone();
        } else {
            self.session = self.committed.clone();
        }
        self.now = self.committed.clone();
    }
}

impl super::Snail {
    /// The transaction ended, committed or rolled back: its modes, its settings, its cursors and
    /// its savepoints with it.
    pub(super) fn ended(&mut self, committed: bool) {
        self.levels.end(committed);
        self.session.end(committed);
        self.savepoints.clear();
    }

    /// A statement of the session's own, run; in a transaction, one that fails fails it.
    pub(super) fn said(&mut self, said: Said, params: &[String]) -> Result<super::Done> {
        let _ = params;
        let kind = said.kind();
        let ran = self.said_run(said);
        match ran {
            Ok(mut done) => {
                if done.statement.is_empty() {
                    done.statement = kind;
                }
                done.generation = self.root.generation;
                Ok(done)
            }
            Err(e) => {
                if self.begun.is_some() && !self.aborted {
                    self.failed()?;
                }
                Err(e)
            }
        }
    }

    fn said_run(&mut self, said: Said) -> Result<super::Done> {
        let done = super::Done::default();
        match said {
            Said::Savepoint { name } => self.savepoint(&name)?,
            Said::RollbackTo { name } => self.rollback_to(&name)?,
            Said::Release { name } => self.release_savepoint(&name)?,
            Said::Chain { commit } => {
                let what = if commit { "COMMIT" } else { "ROLLBACK" };
                if self.begun.is_none() {
                    return Err(Error::new(format!(
                        "{what} AND CHAIN can only be used in transaction blocks"
                    )));
                }
                let modes = self.levels.chained();
                let ended = self.execute(what)?;
                self.execute(&super::statement::begin_text(modes))?;
                return Ok(super::Done {
                    statement: ended.statement,
                    ..ended
                });
            }
            Said::Lock {
                tables,
                mode,
                nowait,
            } => {
                if self.begun.is_none() {
                    return Err(Error::new(
                        "LOCK TABLE can only be used in transaction blocks",
                    ));
                }
                if mode == Mode::AccessExclusive {
                    return Err(Error::new(
                        "LOCK TABLE … IN ACCESS EXCLUSIVE MODE is refused: it would make the \
                         table's readers wait, and a snail's reader never waits, reading the \
                         generation it began on; EXCLUSIVE MODE holds off every writer",
                    ));
                }
                for t in tables {
                    self.lock_table(t, mode, nowait)?;
                }
            }
        }
        Ok(done)
    }
}
