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

/// The byte a relation's holders of a mode mark held, among the block bytes, which the walk for a
/// ring of writers follows.
fn holders_byte(key: u64, mode: Mode) -> u64 {
    lock::block_byte(&(key, "table lock", mode as u8))
}

/// One part of a mode's lock on a table: a byte alone, or a run shared but the writer's own byte;
/// and the mode whose holders it meets.
enum Part {
    Alone(u64, Mode),
    Run(u64, Mode),
}

fn parts(key: u64, mode: Mode, slot: u64) -> Vec<Part> {
    let mut out = Vec::new();
    for (i, &(a, b)) in APART.iter().enumerate() {
        let run = lock::table_run(key, i as u64);
        if a == mode {
            out.push(Part::Alone(run + slot, b));
        }
        if b == mode {
            out.push(Part::Run(run, a));
        }
    }
    if let Some(k) = ALONE.iter().position(|&m| m == mode) {
        out.push(Part::Alone(
            lock::table_run(key, (APART.len() + k) as u64),
            mode,
        ));
    }
    out
}

impl Snail {
    /// The key a table's locks are taken by: the OID the file gave it, which no DROP or DDL of
    /// another moves; for a table the file gave none, a number past every OID by its place. None
    /// for a table the transaction made, which no other writer names until it commits.
    pub(super) fn table_key(&self, table: usize) -> Option<u64> {
        let name = &self.catalog.tables[table].name;
        if self.made_relation(name) {
            return None;
        }
        Some(
            match self.catalog.oids.of(&crate::oids::Object::Class(crate::oids::full(name))) {
                Some(oid) => u64::from(oid),
                None => (1 << 32) + table as u64,
            },
        )
    }

    /// The key a table's blocks, rows and keys are locked and marked by: its lock's key
    /// (`table_key`), which another's DROP of a table laid before it moves no more than its OID; for
    /// a table the transaction made, a key of this writer's own by its name (`made_table_key`).
    pub(super) fn lock_key(&self, table: usize) -> u64 {
        self.table_key(table)
            .unwrap_or_else(|| lock::made_table_key(self.slot, &self.catalog.tables[table].name))
    }

    /// Whether the transaction's DDL made the relation of that name.
    fn made_relation(&self, name: &str) -> bool {
        self.declared.as_ref().is_some_and(|d| d.made_relation(name))
    }

    /// The key a type's locks are taken by, and its name: the OID the file gave it, an array's its
    /// type's. None for a type the words name nowhere in the file, PostgreSQL's own, or one the
    /// transaction made, which no other writer names until it commits.
    fn type_key(&self, words: &str) -> Option<(u64, String)> {
        use crate::oids::Object as O;
        let oid = {
            let _named = crate::oids::named(self.catalog.oids.clone());
            crate::regs::type_oid(words, self.catalog.release).ok()?
        };
        let name = match self.catalog.oids.object(oid)? {
            O::Type(n) | O::Array(n) => n.clone(),
            _ => return None,
        };
        if self.declared.as_ref().is_some_and(|d| d.made_type(&name)) {
            return None;
        }
        let key = self.catalog.oids.of(&O::Type(name.clone()))?;
        Some((u64::from(key), name))
    }

    /// Locks the types a declaration takes out, ACCESS EXCLUSIVE, and those an object new in
    /// `catalog` depends on, ACCESS SHARE, held to the transaction's end, as PostgreSQL locks each
    /// type a DROP takes and each a new dependency is recorded on: by the type's OID, an array by
    /// its type's; a type the transaction made, which no other writer names until it commits, not
    /// at all. None where it waited for no other writer; else the types it waited for to read,
    /// which that writer's end may have taken out.
    pub(super) fn types_locked(
        &mut self,
        catalog: &crate::catalog::Catalog,
    ) -> Result<Option<Vec<String>>> {
        use crate::oids::{full, Object as O};
        let own: Vec<String> = self
            .catalog
            .types
            .iter()
            .map(|t| full(&t.name))
            .chain(self.catalog.domains.iter().map(|d| full(&d.name)))
            .filter(|n| !self.declared.as_ref().is_some_and(|d| d.made_type(n)))
            .collect();
        if own.is_empty() || self.writer.is_none() {
            return Ok(None);
        }
        let stands = |n: &String| {
            catalog.types.iter().any(|t| full(&t.name) == *n)
                || catalog.domains.iter().any(|d| full(&d.name) == *n)
        };
        let mut wanted: Vec<(String, Mode)> = own
            .iter()
            .filter(|n| !stands(n))
            .map(|n| (n.clone(), AccessExclusive))
            .collect();
        for oid in crate::cascade::types_newly_depended_on(&self.catalog, catalog)? {
            let name = match catalog.oids.object(oid) {
                Some(O::Type(n) | O::Array(n)) => n.clone(),
                _ => continue,
            };
            if own.contains(&name) && !wanted.iter().any(|(w, _)| *w == name) {
                wanted.push((name, AccessShare));
            }
        }
        let (mut waited, mut read) = (false, Vec::new());
        for (name, mode) in wanted {
            let Some(oid) = self.catalog.oids.of(&O::Type(name.clone())) else {
                continue;
            };
            if self.lock_relation(u64::from(oid), &name, mode, false)? {
                waited = true;
                if mode == AccessShare {
                    read.push(name);
                }
            }
        }
        Ok(waited.then_some(read))
    }

    /// Takes a table's lock in `mode`, held to the transaction's end, waiting while another writer
    /// holds one that conflicts, as long as it takes, unless the writers waiting make a ring; with
    /// `nowait`, refused at once in PostgreSQL's words. Returns whether it waited.
    pub(super) fn lock_table(&mut self, table: usize, mode: Mode, nowait: bool) -> Result<bool> {
        let Some(key) = self.table_key(table) else {
            return Ok(false);
        };
        let name = self.catalog.tables[table].name.clone();
        self.lock_relation(key, &name, mode, nowait)
    }

    /// Takes the lock of the relation `key` names, `name`, in `mode`, as `lock_table` takes a
    /// table's. Returns whether it waited.
    pub(super) fn lock_relation(
        &mut self,
        key: u64,
        name: &str,
        mode: Mode,
        nowait: bool,
    ) -> Result<bool> {
        if self.session.table_locks.contains(&(key, mode)) || self.writer.is_none() {
            return Ok(false);
        }
        let file = self.writer.as_ref().expect("a snail open to write").file();
        let mut taken = Vec::new();
        let mut waited = false;
        let waiting = if nowait { Waiting::Try } else { Waiting::Wait };
        for p in parts(key, mode, self.slot) {
            let got = match &p {
                Part::Alone(b, meets) => self.wait_for(*meets, key, waiting, &mut waited, |wait| {
                    lock::lock_as(file, *b, false, wait)
                }),
                Part::Run(r, meets) => self.wait_for(*meets, key, waiting, &mut waited, |wait| {
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
                        name.rsplit('.').next().unwrap_or_default()
                    )));
                }
            }
        }
        lock::mark_held(file, self.slot, holders_byte(key, mode), true)?;
        self.session.table_locks.insert((key, mode));
        Ok(waited)
    }

    /// Takes one part of a relation's lock: at once where it can, else, with `Wait`, marked waiting
    /// for the holders of the mode it meets, failed where they wait in a ring for this writer.
    fn wait_for(
        &self,
        meets: Mode,
        key: u64,
        waiting: Waiting,
        waited: &mut bool,
        take: impl Fn(bool) -> Result<bool>,
    ) -> Result<bool> {
        if take(false)? {
            return Ok(true);
        }
        if waiting == Waiting::Try {
            return Ok(false);
        }
        *waited = true;
        let file = self.writer.as_ref().expect("a snail open to write").file();
        let byte = holders_byte(key, meets);
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

    /// Locks the relations a declaration changes, each in the mode PostgreSQL takes for it
    /// (`ddl::locked`), held to the transaction's end: each named on the catalog the transaction
    /// declares on, an index by its table, a schema by itself and every relation in it, a type by
    /// its OID (`type_key`), and a name
    /// no relation holds not at all, the declaration refusing it in PostgreSQL's words. Returns
    /// whether it waited for another writer's.
    pub(super) fn ddl_locked(&mut self, ddl: &str) -> Result<bool> {
        self.relations_locked(crate::ddl::locked(ddl))
    }

    /// Locks ACCESS SHARE, held to the transaction's end, each relation a declaration reads as
    /// `catalog` declares it, as PostgreSQL's parse analysis opens them: those a view or a SQL
    /// function's body it made reads, at every level, and the table a sequence's OWNED BY names;
    /// and the schema each object it makes is made in. Returns whether it waited for another
    /// writer's.
    pub(super) fn reads_locked(&mut self, catalog: &crate::catalog::Catalog) -> Result<bool> {
        use crate::ddl::Locked;
        use crate::deparse::depend::relations_read;
        let _named = crate::oids::named(catalog.oids.clone());
        let cat = crate::deparse::cat::Cat::of(catalog);
        let mut read: Vec<u32> = Vec::new();
        // The relations a question deparse does not read names, by their full names.
        let mut named: Vec<String> = Vec::new();
        for v in &catalog.views {
            if self.catalog.views.iter().any(|x| x.name == v.name && x.query == v.query) {
                continue;
            }
            match crate::deparse::query::view_query(&cat, v) {
                Ok((q, _)) => read.extend(relations_read(&cat, &q)),
                Err(_) => {
                    let bound = crate::grammar::bound(&v.query);
                    let parsed = super::image::parse_sql(bound.as_deref().unwrap_or(&v.query))?;
                    named.extend(named_in(&parsed));
                }
            }
        }
        for f in &catalog.functions {
            let kept = self.catalog.functions.iter().any(|x| {
                x.name == f.name && x.args == f.args && x.statement == f.statement && x.form == f.form
            });
            if kept {
                continue;
            }
            // A body naming a relation that does not stand is refused, as its parse analysis is.
            // A body in quotes is analyzed as it is made, and one deparse does not read is still
            // opened: each locks the relations its expression names.
            match crate::deparse::funcs::own_body(&cat, f) {
                Ok(Some(q)) => read.extend(relations_read(&cat, &q)),
                Err(e) if e.sqlstate() == Some("42P01") => return Err(e),
                Ok(None) | Err(_) => named.extend(named_in(&f.body)),
            }
        }
        let mut wanted: Vec<(Locked, Mode)> = Vec::new();
        let mut want = |n: &str| {
            let l = Locked::Relation(n.to_string());
            if !wanted.iter().any(|(w, _)| *w == l) {
                wanted.push((l, AccessShare));
            }
        };
        for oid in read {
            if let Some(crate::oids::Object::Class(n)) = catalog.oids.object(oid) {
                want(n);
            }
        }
        for n in &named {
            let stands = catalog.tables.iter().any(|t| crate::oids::full(&t.name) == *n)
                || catalog.views.iter().any(|v| crate::oids::full(&v.name) == *n)
                || catalog.sequences.iter().any(|s| crate::oids::full(&s.name) == *n);
            if stands {
                want(n);
            }
        }
        for s in &catalog.sequences {
            let was = self.catalog.sequence(&s.name).map(|x| &x.owned_by);
            if let Some((t, _)) = s.owned_by.as_ref().filter(|_| was != Some(&s.owned_by)) {
                wanted.push((Locked::Relation(t.clone()), AccessShare));
            }
        }
        // The schema of each object it makes, as PostgreSQL locks what a new object depends on: a
        // table, a view, a sequence, a type or a function; an index stands under its table's lock.
        for o in super::change::names_made(&self.catalog, catalog) {
            use crate::oids::{full, parts, Object};
            let relation = |n: &str| {
                catalog.tables.iter().any(|t| full(&t.name) == n)
                    || catalog.views.iter().any(|v| full(&v.name) == n)
                    || catalog.sequences.iter().any(|s| full(&s.name) == n)
            };
            let made_in = match &o {
                Object::Class(n) if relation(n) => n,
                Object::Type(n) | Object::Proc(n, _) => n,
                _ => continue,
            };
            let l = Locked::Namespace(parts(made_in).0.to_string());
            if !wanted.iter().any(|(w, _)| *w == l) {
                wanted.push((l, AccessShare));
            }
        }
        self.relations_locked(wanted)
    }

    /// Locks each relation named in its mode, as `ddl_locked` describes, held to the transaction's
    /// end. Returns whether it waited for another writer's.
    fn relations_locked(&mut self, locked: Vec<(crate::ddl::Locked, Mode)>) -> Result<bool> {
        use crate::ddl::Locked;
        use crate::oids::{full, parts, Object};
        let mut keys: Vec<(u64, String, Mode)> = Vec::new();
        for (l, mode) in locked {
            let (schema, name) = match &l {
                Locked::Relation(n) => (None, full(n)),
                Locked::Schema(s) => (Some(s.clone()), String::new()),
                // A schema itself, by its OID: but one the transaction made, which no other
                // writer names until it commits, or one the catalog does not hold.
                Locked::Namespace(s) => {
                    let made = self.declared.as_ref().is_some_and(|d| d.made_schema(s));
                    let oid = match s.as_str() {
                        "public" => self.catalog.oids.public(),
                        _ => self.catalog.oids.of(&Object::Namespace(s.clone())),
                    };
                    if let Some(oid) = oid.filter(|_| !made) {
                        keys.push((u64::from(oid), s.clone(), mode));
                    }
                    continue;
                }
                // A type by its OID, as `type_key` finds it.
                Locked::Type(words) => {
                    if let Some((key, name)) = self.type_key(words) {
                        keys.push((key, name, mode));
                    }
                    continue;
                }
            };
            let named = |n: &str| match &schema {
                Some(s) => parts(&full(n)).0 == s,
                None => full(n) == name,
            };
            for (ti, t) in self.catalog.tables.iter().enumerate() {
                let whole = full(&t.name);
                let (schema_of, _) = parts(&whole);
                let index = t
                    .orders
                    .iter()
                    .map(|o| &o.name)
                    .chain(t.brins.iter().map(|b| &b.name))
                    .any(|o| schema.is_none() && full(&format!("{schema_of}.{o}")) == name);
                if let Some(k) = self.table_key(ti).filter(|_| t.declared() && (named(&t.name) || index)) {
                    keys.push((k, t.name.clone(), mode));
                }
                // An index held as an element strand locks the table it is of.
                let of = t.elements_of.as_ref().filter(|_| schema.is_none() && named(&t.name));
                let oi = of.and_then(|o| self.catalog.tables.iter().position(|x| x.name == *o));
                if let Some((oi, k)) = oi.and_then(|oi| self.table_key(oi).map(|k| (oi, k))) {
                    keys.push((k, self.catalog.tables[oi].name.clone(), mode));
                }
            }
            let others = self
                .catalog
                .views
                .iter()
                .map(|v| &v.name)
                .chain(self.catalog.sequences.iter().map(|s| &s.name));
            for n in others.filter(|n| named(n) && !self.made_relation(n)) {
                if let Some(oid) = self.catalog.oids.of(&Object::Class(full(n))) {
                    keys.push((u64::from(oid), n.clone(), mode));
                }
            }
        }
        let mut waited = false;
        for (key, name, mode) in keys {
            waited |= self.lock_relation(key, &name, mode, false)?;
        }
        Ok(waited)
    }

    /// Lets go of a relation's lock in one mode.
    fn let_go_table(&mut self, key: u64, mode: Mode) -> Result<()> {
        if !self.session.table_locks.remove(&(key, mode)) {
            return Ok(());
        }
        let Some(w) = self.writer.as_ref() else {
            return Ok(());
        };
        lock::mark_held(w.file(), self.slot, holders_byte(key, mode), false)?;
        for p in parts(key, mode, self.slot) {
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
        kept: &std::collections::BTreeSet<(u64, Mode)>,
    ) -> Result<()> {
        let since: Vec<(u64, Mode)> = self
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

/// The relations a statement's text names as FROM items, at every level, each by its full name: a
/// name alone in public, a WITH query's name passed over, a function in FROM not one. What its parse
/// analysis opens, read where deparse does not read the question.
fn named_in(node: &impl sqlparser::ast::Visit) -> Vec<String> {
    use crate::deparse::analyze::folded;
    use sqlparser::ast::{Query, TableFactor, Visitor};
    use std::ops::ControlFlow;
    #[derive(Default)]
    struct Named {
        with: Vec<String>,
        items: Vec<Vec<String>>,
    }
    impl Visitor for Named {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            for c in q.with.iter().flat_map(|w| &w.cte_tables) {
                self.with.push(folded(&c.alias.name));
            }
            ControlFlow::Continue(())
        }
        fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
            if let TableFactor::Table {
                name, args: None, ..
            } = t
            {
                self.items
                    .push(name.0.iter().filter_map(|p| p.as_ident().map(folded)).collect());
            }
            ControlFlow::Continue(())
        }
    }
    let mut v = Named::default();
    let _ = node.visit(&mut v);
    v.items
        .iter()
        .filter_map(|parts| match parts.as_slice() {
            [n] if v.with.contains(n) => None,
            [n] => Some(format!("public.{n}")),
            [s, n] => Some(format!("{s}.{n}")),
            _ => None,
        })
        .collect()
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
