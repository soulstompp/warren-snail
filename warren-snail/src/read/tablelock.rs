//! A table's locks, as PostgreSQL's modes conflict: LOCK TABLE takes one, a writer of the table ROW
//! EXCLUSIVE and a question locking its rows ROW SHARE, each held to the transaction's end, or the
//! statement's where it stands alone. A reader takes none, since a snail's reader never waits.

use super::change::Waiting;
use super::Snail;
use crate::lock;
use crate::{Error, Result};

/// PostgreSQL's table lock modes, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Mode {
    AccessShare,
    RowShare,
    RowExclusive,
    ShareUpdateExclusive,
    Share,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
}

use Mode::*;

/// The modes that conflict, two apart, as PostgreSQL's table of conflicts has them: the first takes
/// its slot's byte of their run alone, the second the whole run shared, so that holders of either
/// one alone never meet.
const APART: [(Mode, Mode); 17] = [
    (AccessShare, AccessExclusive),
    (RowShare, Exclusive),
    (RowShare, AccessExclusive),
    (RowExclusive, Share),
    (RowExclusive, ShareRowExclusive),
    (RowExclusive, Exclusive),
    (RowExclusive, AccessExclusive),
    (ShareUpdateExclusive, Share),
    (ShareUpdateExclusive, ShareRowExclusive),
    (ShareUpdateExclusive, Exclusive),
    (ShareUpdateExclusive, AccessExclusive),
    (Share, ShareRowExclusive),
    (Share, Exclusive),
    (Share, AccessExclusive),
    (ShareRowExclusive, Exclusive),
    (ShareRowExclusive, AccessExclusive),
    (Exclusive, AccessExclusive),
];

/// The modes that conflict with themselves, each a byte taken alone.
const ALONE: [Mode; 4] = [
    ShareUpdateExclusive,
    ShareRowExclusive,
    Exclusive,
    AccessExclusive,
];

impl Mode {
    /// The mode as LOCK TABLE names it.
    pub fn words(self) -> &'static str {
        match self {
            AccessShare => "ACCESS SHARE",
            RowShare => "ROW SHARE",
            RowExclusive => "ROW EXCLUSIVE",
            ShareUpdateExclusive => "SHARE UPDATE EXCLUSIVE",
            Share => "SHARE",
            ShareRowExclusive => "SHARE ROW EXCLUSIVE",
            Exclusive => "EXCLUSIVE",
            AccessExclusive => "ACCESS EXCLUSIVE",
        }
    }

    /// Whether two modes conflict, as PostgreSQL's table of conflicts has them.
    pub fn conflicts(self, other: Mode) -> bool {
        (self == other && ALONE.contains(&self))
            || APART
                .iter()
                .any(|&(a, b)| (a, b) == (self, other) || (a, b) == (other, self))
    }
}

/// The byte a table's holders of a mode mark held, among the block bytes, which the walk for a ring
/// of writers follows.
fn holders_byte(table: usize, mode: Mode) -> u64 {
    lock::block_byte(&(table as u64, "table lock", mode as u8))
}

/// One part of a mode's lock on a table: a byte alone, or a run shared but the writer's own byte;
/// and the mode whose holders it meets.
enum Part {
    Alone(u64, Mode),
    Run(u64, Mode),
}

fn parts(table: usize, mode: Mode, slot: u64) -> Vec<Part> {
    let mut out = Vec::new();
    for (i, &(a, b)) in APART.iter().enumerate() {
        let run = lock::table_run(table, i as u64);
        if a == mode {
            out.push(Part::Alone(run + slot, b));
        }
        if b == mode {
            out.push(Part::Run(run, a));
        }
    }
    if let Some(k) = ALONE.iter().position(|&m| m == mode) {
        out.push(Part::Alone(
            lock::table_run(table, (APART.len() + k) as u64),
            mode,
        ));
    }
    out
}

impl Snail {
    /// Takes a table's lock in `mode`, held to the transaction's end, waiting while another writer
    /// holds one that conflicts, as long as it takes, unless the writers waiting make a ring; with
    /// `nowait`, refused at once in PostgreSQL's words.
    pub(super) fn lock_table(&mut self, table: usize, mode: Mode, nowait: bool) -> Result<()> {
        if self.session.table_locks.contains(&(table, mode)) || self.writer.is_none() {
            return Ok(());
        }
        let file = self.writer.as_ref().expect("a snail open to write").file();
        let mut taken = Vec::new();
        let waiting = if nowait { Waiting::Try } else { Waiting::Wait };
        for p in parts(table, mode, self.slot) {
            let got = match &p {
                Part::Alone(b, meets) => self.wait_for(*b, *meets, table, waiting, |wait| {
                    lock::lock_as(file, *b, false, wait)
                }),
                Part::Run(r, meets) => self.wait_for(*r, *meets, table, waiting, |wait| {
                    lock::lock_run_shared(file, *r, self.slot, wait)
                }),
            };
            match got {
                Ok(true) => taken.push(p),
                Ok(false) | Err(_) => {
                    for t in taken {
                        self.let_go_part(&t)?;
                    }
                    got?;
                    return Err(Error::new(format!(
                        "could not obtain lock on relation \"{}\"",
                        self.catalog.tables[table]
                            .name
                            .rsplit('.')
                            .next()
                            .unwrap_or_default()
                    )));
                }
            }
        }
        lock::mark_held(file, self.slot, holders_byte(table, mode), true)?;
        self.session.table_locks.insert((table, mode));
        Ok(())
    }

    /// Takes one part of a table's lock: at once where it can, else, with `Wait`, marked waiting for
    /// the holders of the mode it meets, failed where they wait in a ring for this writer.
    fn wait_for(
        &self,
        _at: u64,
        meets: Mode,
        table: usize,
        waiting: Waiting,
        take: impl Fn(bool) -> Result<bool>,
    ) -> Result<bool> {
        if take(false)? {
            return Ok(true);
        }
        if waiting == Waiting::Try {
            return Ok(false);
        }
        let file = self.writer.as_ref().expect("a snail open to write").file();
        let byte = holders_byte(table, meets);
        lock::mark_waiting(file, self.slot, byte, true)?;
        let (group, me) = self.group.as_ref().expect("a snail open to write");
        let ring = group.ring(*me, byte);
        if !matches!(ring, Ok(false)) {
            lock::mark_waiting(file, self.slot, byte, false)?;
            ring?;
            return Err(Error::new(
                "deadlock detected: this writer waits for a table's lock held by a writer that \
                 waits, in a ring, for a lock this writer holds",
            ));
        }
        let got = super::settings::wait_for_lock(&take);
        lock::mark_waiting(file, self.slot, byte, false)?;
        got
    }

    fn let_go_part(&self, p: &Part) -> Result<()> {
        let file = self.writer.as_ref().expect("a snail open to write").file();
        match p {
            Part::Alone(b, _) => lock::unlock_block(file, *b),
            Part::Run(r, _) => lock::unlock_run_shared(file, *r, self.slot),
        }
    }

    /// Lets go of a table's lock in one mode.
    fn let_go_table(&mut self, table: usize, mode: Mode) -> Result<()> {
        if !self.session.table_locks.remove(&(table, mode)) {
            return Ok(());
        }
        let Some(w) = self.writer.as_ref() else {
            return Ok(());
        };
        lock::mark_held(w.file(), self.slot, holders_byte(table, mode), false)?;
        for p in parts(table, mode, self.slot) {
            self.let_go_part(&p)?;
        }
        Ok(())
    }

    /// Lets go of every table's lock the transaction or the statement took.
    pub(super) fn let_go_tables(&mut self) -> Result<()> {
        for (t, m) in std::mem::take(&mut self.session.table_locks) {
            self.session.table_locks.insert((t, m));
            self.let_go_table(t, m)?;
        }
        Ok(())
    }

    /// Lets go of the tables' locks taken since a savepoint that held `kept`.
    pub(super) fn table_locks_back_to(
        &mut self,
        kept: &std::collections::BTreeSet<(usize, Mode)>,
    ) -> Result<()> {
        let since: Vec<(usize, Mode)> = self
            .session
            .table_locks
            .iter()
            .filter(|l| !kept.contains(l))
            .copied()
            .collect();
        for (t, m) in since {
            self.let_go_table(t, m)?;
        }
        Ok(())
    }
}

/// LOCK [TABLE] [ONLY] name [*] [, …] [IN mode MODE] [NOWAIT], read off its words as PostgreSQL's
/// grammar takes them: none where the words are no such statement.
pub(super) fn parse_lock(
    tokens: &[sqlparser::tokenizer::Token],
) -> Option<Result<(Vec<String>, Mode, bool)>> {
    use sqlparser::tokenizer::Token;
    let words: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let keyword = |i: usize| match words.get(i) {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_uppercase(),
        _ => String::new(),
    };
    if keyword(0) != "LOCK" {
        return None;
    }
    let syntax = |i: usize| {
        Err(Error::new(match words.get(i) {
            Some(t) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".to_string(),
        }))
    };
    let mut i = 1;
    if keyword(i) == "TABLE" {
        i += 1;
    }
    let mut names = Vec::new();
    loop {
        if keyword(i) == "ONLY" {
            i += 1;
        }
        let mut name = Vec::new();
        loop {
            match words.get(i) {
                Some(Token::Word(w)) if w.quote_style.is_some() => name.push(w.value.clone()),
                Some(Token::Word(w))
                    if !name.is_empty() || !matches!(keyword(i).as_str(), "IN" | "NOWAIT") =>
                {
                    name.push(w.value.to_lowercase())
                }
                _ => return Some(syntax(i)),
            }
            i += 1;
            if matches!(words.get(i), Some(Token::Period)) {
                i += 1;
                continue;
            }
            break;
        }
        if matches!(words.get(i), Some(Token::Mul)) {
            i += 1;
        }
        names.push(name.join("."));
        if matches!(words.get(i), Some(Token::Comma)) {
            i += 1;
            continue;
        }
        break;
    }
    let mut mode = AccessExclusive;
    if keyword(i) == "IN" {
        i += 1;
        let start = i;
        let mut said = Vec::new();
        while !matches!(keyword(i).as_str(), "MODE" | "") {
            said.push(keyword(i));
            i += 1;
        }
        if keyword(i) != "MODE" {
            return Some(syntax(i));
        }
        mode = match said.join(" ").as_str() {
            "ACCESS SHARE" => AccessShare,
            "ROW SHARE" => RowShare,
            "ROW EXCLUSIVE" => RowExclusive,
            "SHARE UPDATE EXCLUSIVE" => ShareUpdateExclusive,
            "SHARE" => Share,
            "SHARE ROW EXCLUSIVE" => ShareRowExclusive,
            "EXCLUSIVE" => Exclusive,
            "ACCESS EXCLUSIVE" => AccessExclusive,
            _ => return Some(syntax(start + said.len().min(1))),
        };
        i += 1;
    }
    let mut nowait = false;
    if keyword(i) == "NOWAIT" {
        nowait = true;
        i += 1;
    }
    if matches!(words.get(i), Some(Token::SemiColon)) {
        i += 1;
    }
    if i < words.len() {
        return Some(syntax(i));
    }
    Some(Ok((names, mode, nowait)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_modes_conflict_as_postgresqls_table_has_them() {
        // lock.c's LockConflicts, a row a mode: the modes each conflicts with.
        let table: [(Mode, &[Mode]); 8] = [
            (AccessShare, &[AccessExclusive]),
            (RowShare, &[Exclusive, AccessExclusive]),
            (
                RowExclusive,
                &[Share, ShareRowExclusive, Exclusive, AccessExclusive],
            ),
            (
                ShareUpdateExclusive,
                &[
                    ShareUpdateExclusive,
                    Share,
                    ShareRowExclusive,
                    Exclusive,
                    AccessExclusive,
                ],
            ),
            (
                Share,
                &[
                    RowExclusive,
                    ShareUpdateExclusive,
                    ShareRowExclusive,
                    Exclusive,
                    AccessExclusive,
                ],
            ),
            (
                ShareRowExclusive,
                &[
                    RowExclusive,
                    ShareUpdateExclusive,
                    Share,
                    ShareRowExclusive,
                    Exclusive,
                    AccessExclusive,
                ],
            ),
            (
                Exclusive,
                &[
                    RowShare,
                    RowExclusive,
                    ShareUpdateExclusive,
                    Share,
                    ShareRowExclusive,
                    Exclusive,
                    AccessExclusive,
                ],
            ),
            (
                AccessExclusive,
                &[
                    AccessShare,
                    RowShare,
                    RowExclusive,
                    ShareUpdateExclusive,
                    Share,
                    ShareRowExclusive,
                    Exclusive,
                    AccessExclusive,
                ],
            ),
        ];
        for (m, with) in table {
            for (n, _) in table {
                assert_eq!(m.conflicts(n), with.contains(&n), "{m:?} against {n:?}");
            }
        }
    }

    #[test]
    fn lock_table_is_read_off_its_words() {
        let read = |s: &str| {
            let t =
                sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, s)
                    .tokenize()
                    .unwrap();
            parse_lock(&t).map(|r| r.map_err(|e| e.to_string()))
        };
        assert_eq!(
            read("LOCK TABLE s.a"),
            Some(Ok((vec!["s.a".into()], AccessExclusive, false)))
        );
        assert_eq!(
            read("lock only a *, \"B\" in share row exclusive mode nowait"),
            Some(Ok((vec!["a".into(), "B".into()], ShareRowExclusive, true)))
        );
        assert!(matches!(read("LOCK a IN SHARED MODE"), Some(Err(_))));
        assert_eq!(read("SELECT 1"), None);
    }
}
