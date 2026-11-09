//! Several snails opened together, each table in one of them. A round across them reads every
//! snail's root, then every root again, and takes them where none changed between, so it reads one
//! moment of them all and waits on no lock. A question names their tables by schema, each name held
//! by one of them; a join across them reads each table from the snail that holds it.
//!
//! A transaction writes into several, one root deciding. Its pages go into every snail it writes,
//! and each other snail's new root into that snail's other copy of the root, marked pending; then
//! the deciding snail's root, which names the others' new roots; then each of them, taken. A snail
//! read alone never takes a pending copy, and a writer of it alone takes no commit while one lies
//! beside its root. A round across them takes a pending copy the deciding root names as written;
//! one open to write writes it, and lets go of one the deciding root never came to name.

use super::question;
use super::statement::{self, Statement};
use super::{join, Done, Round, Snail};
use crate::catalog::Catalog;
use crate::cut::PAGE;
use crate::file::{decode_carried, encode_carried, Decision, Pending, Root, Writer, Written};
use crate::lock;
use crate::{Error, Result};
use arrow::array::RecordBatch;
use std::path::Path;

/// Where a commit across snails stops short, for the tests of a crash: with every other snail's root
/// written as a pending copy, before the deciding root; after the deciding root, before the others'
/// roots are taken; or halfway through the first other snail's root.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum StopAcross {
    BeforeDecision,
    AfterDecision,
    HalfFinal,
}

/// A snail's first file opened again only to read its locks, so that they show every writer's,
/// this writer's own as another's; and the file's device and inode.
pub(crate) struct Seen {
    file: std::fs::File,
    at: crate::file::Identity,
}

impl Seen {
    fn open(path: &Path) -> Result<Seen> {
        let file = std::fs::File::open(path)
            .map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
        Ok(Seen {
            at: crate::file::identity(&file)
                .map_err(|e| Error::new(format!("{}: {e}", path.display())))?,
            file,
        })
    }
}

/// What a writer shares among the snails it writes, so that waiting for a block it finds a ring
/// through any snail's file: each of its snails' first files seen afresh, and the slot it holds in
/// each.
pub(crate) struct Group {
    files: Vec<Seen>,
    slot: u64,
}

impl Group {
    /// What the writer of one snail shares: its file, seen afresh, and its slot there.
    pub(crate) fn alone(path: &Path, slot: u64) -> Result<Group> {
        Ok(Group {
            files: vec![Seen::open(path)?],
            slot,
        })
    }

    fn seen<'a>(&'a self, reached: &'a [Seen], f: usize) -> &'a Seen {
        match f.checked_sub(self.files.len()) {
            Some(r) => &reached[r],
            None => &self.files[f],
        }
    }

    /// The file a path names: one this writer writes or the walk has reached already, by its device
    /// and inode, or one opened now; none where it cannot be opened, and the walk passes it over.
    fn reach(&self, reached: &mut Vec<Seen>, path: &[u8]) -> Option<usize> {
        use std::os::unix::ffi::OsStrExt;
        let seen = Seen::open(Path::new(std::ffi::OsStr::from_bytes(path))).ok()?;
        if let Some(f) = self
            .files
            .iter()
            .chain(reached.iter())
            .position(|s| s.at == seen.at)
        {
            return Some(f);
        }
        reached.push(seen);
        Some(self.files.len() + reached.len() - 1)
    }

    /// Whether waiting for the block of lock byte `b` in the `me`th file closes a ring: its holder
    /// waits for a block, in that file or in another it writes, whose holder waits, and so on, back
    /// to a block this writer holds. A file this writer never opened is reached where a writer of
    /// several marks it beside the file the walk stands in.
    pub(crate) fn ring(&self, me: usize, b: u64) -> Result<bool> {
        let mut reached = Vec::new();
        let mut todo = vec![(me, b)];
        let mut seen = std::collections::BTreeSet::new();
        // A byte held shared has a holder for each writer sharing it, and the walk follows each.
        while let Some((f, byte)) = todo.pop() {
            for holder in lock::holders(&self.seen(&reached, f).file, byte)? {
                if f < self.files.len() && holder == self.slot {
                    return Ok(true);
                }
                let of = lock::writer_of(&self.seen(&reached, f).file, holder)?;
                let who = match of {
                    Some(id) => (Some(id), (0, 0), 0),
                    None => (None, self.seen(&reached, f).at, holder),
                };
                if !seen.insert(who) {
                    continue;
                }
                // A writer of one snail waits only in its file; a writer of several, the same slot
                // in each of their files, waits in this one or in one it marks beside it here.
                let mut next = lock::awaited(&self.seen(&reached, f).file, holder)?.map(|a| (f, a));
                if let (None, Some(id)) = (next, of) {
                    for path in lock::beside(&self.seen(&reached, f).file, holder)? {
                        let Some(g) = self.reach(&mut reached, &path) else {
                            continue;
                        };
                        let file = &self.seen(&reached, g).file;
                        if lock::writer_of(file, holder)? == Some(id) {
                            if let Some(a) = lock::awaited(file, holder)? {
                                next = Some((g, a));
                                break;
                            }
                        }
                    }
                }
                todo.extend(next);
            }
        }
        Ok(false)
    }
}

/// Several snails opened together for rounds across them.
pub struct Snails {
    snails: Vec<Snail>,
    /// Their files, as opened.
    paths: Vec<std::path::PathBuf>,
    /// Every snail's catalog as one, each table as its snail holds it: what a question across them
    /// is read against.
    catalog: Catalog,
    /// For each table of `catalog`, its snail and its place among that snail's tables.
    at: Vec<(usize, usize)>,
    /// A transaction begun across them, and whether one of its statements failed.
    begun: bool,
    aborted: bool,
    /// The snails the transaction under way has written, in the order its statements first wrote
    /// them: the first decides.
    written: Vec<usize>,
    /// The savepoints of the transaction under way, oldest first, each with the snails it had
    /// written then.
    savepoints: Vec<(String, Vec<usize>, Vec<u64>)>,
    /// Their session's prepared statements and cursors, which read across them.
    session: super::session::Session,
    stop: Option<StopAcross>,
    /// Run once, after the first snail's root is read in the next round's first pass: for the tests
    /// of a commit landing between the passes.
    between: Option<Box<dyn FnMut()>>,
    writing: bool,
    /// The functions their caller has handed them, which their questions call.
    functions: std::rc::Rc<Vec<std::rc::Rc<super::handed::Function>>>,
}

impl Snails {
    /// An answer's rows as `answer_text` writes them, under the session's TimeZone as it stands,
    /// which stands alike on every snail, and an OID of a file's own objects in a reg* type by its
    /// name where one snail is open.
    pub fn text_of(&self, batch: &RecordBatch) -> Result<Vec<Vec<Option<String>>>> {
        let _zoned = self.snails[0].in_zone();
        let _named = match self.snails.as_slice() {
            [one] => crate::oids::named(one.catalog.oids.clone()),
            _ => crate::oids::named_several(),
        };
        super::answer_text(batch)
    }

    /// Opens several snails together to read. Built `standalone`, each holds `HELD_BYTES` of the
    /// pages its rounds read.
    pub fn open(paths: &[&Path]) -> Result<Snails> {
        let snails = paths
            .iter()
            .map(|p| Snail::open(p))
            .collect::<Result<_>>()?;
        Snails::of(snails, paths, false)
    }

    /// Opens several snails together to read, each holding up to `bytes` of the pages its rounds
    /// read, between rounds.
    #[cfg(feature = "standalone")]
    pub fn open_holding(paths: &[&Path], bytes: usize) -> Result<Snails> {
        let snails = paths
            .iter()
            .map(|p| Snail::open_holding(p, bytes))
            .collect::<Result<_>>()?;
        Snails::of(snails, paths, false)
    }

    /// Opens several snails together to write, as one writer of all of them, which holds the same
    /// slot in each of their files. A root one of them holds written for a transaction across them
    /// is written now, or let go where the root that would have decided it never came to.
    pub fn open_writing(paths: &[&Path]) -> Result<Snails> {
        #[cfg(feature = "standalone")]
        {
            let mut snails = Vec::with_capacity(paths.len());
            for p in paths {
                let (writer, _) = Writer::open(p)?;
                let mut snail = Snail::open(p)?;
                snail.writer = Some(writer);
                snails.push(snail);
            }
            let files: Vec<&std::fs::File> = snails
                .iter()
                .map(|s| s.writer.as_ref().unwrap().file())
                .collect();
            let id = lock::writer_id();
            let slot = lock::take_slot_across(&files, id)?;
            // In each file, the path of every other, for a writer that never opened them.
            let named: Vec<Vec<u8>> = paths
                .iter()
                .map(|p| {
                    use std::os::unix::ffi::OsStringExt;
                    std::fs::canonicalize(p)
                        .map(|c| c.into_os_string().into_vec())
                        .map_err(|e| Error::new(format!("{}: {e}", p.display())))
                })
                .collect::<Result<_>>()?;
            for (i, f) in files.iter().enumerate() {
                let others: Vec<&[u8]> = (0..named.len())
                    .filter(|&j| j != i)
                    .map(|j| named[j].as_slice())
                    .collect();
                lock::mark_beside(f, slot, &others)?;
            }
            let group = std::rc::Rc::new(Group {
                files: paths.iter().map(|p| Seen::open(p)).collect::<Result<_>>()?,
                slot,
            });
            for (i, s) in snails.iter_mut().enumerate() {
                s.slot = slot;
                s.group = Some((group.clone(), i));
            }
            let mut out = Snails::of(snails, paths, true)?;
            out.resolve()?;
            Ok(out)
        }
        #[cfg(not(feature = "standalone"))]
        {
            let _ = paths;
            Err(Error::new(
                "a snail built without `standalone` writes nothing: inside PostgreSQL its commit \
                 would have to be PostgreSQL's",
            ))
        }
    }

    fn of(snails: Vec<Snail>, paths: &[&Path], writing: bool) -> Result<Snails> {
        let (catalog, at) = {
            let refs: Vec<&Snail> = snails.iter().collect();
            Snails::merged(&refs, &|i| paths[i].display().to_string())?
        };
        // A round across them holds its rows and the pages it keeps in one room.
        for s in &snails[1..] {
            s.reader.hold_rounds_in(snails[0].reader.room());
        }
        Ok(Snails {
            snails,
            paths: paths.iter().map(|p| p.to_path_buf()).collect(),
            catalog,
            at,
            begun: false,
            aborted: false,
            written: Vec::new(),
            savepoints: Vec::new(),
            session: Default::default(),
            stop: None,
            between: None,
            writing,
            functions: Default::default(),
        })
    }

    /// Every snail's catalog as one, each table as its snail holds it, and for each table its snail
    /// and its place among that snail's tables: refused where the snails come from two releases, or
    /// two of them hold one name, as one database holding every snail's tables would refuse it.
    /// `shown` names a snail in a refusal.
    pub(super) fn merged(
        snails: &[&Snail],
        shown: &dyn Fn(usize) -> String,
    ) -> Result<(Catalog, Vec<(usize, usize)>)> {
        if snails.is_empty() {
            return Err(Error::new("no snail to open"));
        }
        // One database holds every snail's tables, so one release's OIDs, and each name once.
        if let Some(i) = (1..snails.len()).find(|&i| snails[i].release() != snails[0].release()) {
            return Err(Error::new(format!(
                "snails opened together come from one PostgreSQL release: {}'s OIDs are {}'s, \
                 and {}'s {}'s",
                shown(0),
                snails[0].release(),
                shown(i),
                snails[i].release()
            )));
        }
        let mut relations: std::collections::BTreeMap<String, usize> = Default::default();
        let mut objects: std::collections::BTreeMap<String, (usize, String)> = Default::default();
        for (i, s) in snails.iter().enumerate() {
            let c = &s.catalog;
            let mut names: Vec<String> = c.sequences.iter().map(|q| q.name.clone()).collect();
            names.extend(c.views.iter().map(|v| v.name.clone()));
            for t in &c.tables {
                names.push(t.name.clone());
                let schema = t.name.rsplit_once('.').map_or("", |(s, _)| s);
                for o in &t.orders {
                    names.push(match schema {
                        "" => o.name.clone(),
                        s => format!("{s}.{}", o.name),
                    });
                }
            }
            names.sort();
            names.dedup();
            for n in names {
                if let Some(&j) = relations.get(&n) {
                    return Err(Error::new(format!(
                        "relation \"{n}\" is held by {} and by {}: snails opened together hold \
                         each name once",
                        shown(j),
                        shown(i)
                    )));
                }
                relations.insert(n, i);
            }
            // A type, a domain, a function or a collation two snails declare alike is one object.
            let declared = c
                .types
                .iter()
                .map(|t| (format!("type {}", t.name), format!("{t:?}")))
                .chain(
                    c.domains
                        .iter()
                        .map(|d| (format!("type {}", d.name), format!("{d:?}"))),
                )
                .chain(c.functions.iter().map(|f| {
                    (
                        format!("function {}({:?})", f.name, f.args),
                        format!("{f:?}"),
                    )
                }))
                .chain(
                    c.collations
                        .iter()
                        .map(|k| (format!("collation {}", k.name), format!("{k:?}"))),
                );
            for (name, def) in declared {
                match objects.get(&name) {
                    Some((j, d)) if *d != def => {
                        return Err(Error::new(format!(
                            "{name} is declared otherwise by {} and by {}: snails opened \
                             together hold each name once",
                            shown(*j),
                            shown(i)
                        )))
                    }
                    Some(_) => {}
                    None => {
                        objects.insert(name, (i, def));
                    }
                }
            }
        }
        let mut catalog = snails[0].catalog.clone();
        let mut at: Vec<(usize, usize)> = (0..catalog.tables.len()).map(|t| (0, t)).collect();
        for (i, s) in snails.iter().enumerate().skip(1) {
            let c = &s.catalog;
            catalog.ddl = format!("{}\n{}", catalog.ddl, c.ddl);
            for (t, table) in c.tables.iter().enumerate() {
                catalog.tables.push(table.clone());
                at.push((i, t));
            }
            catalog.sequences.extend(c.sequences.iter().cloned());
            catalog.views.extend(c.views.iter().cloned());
            for t in &c.types {
                if !catalog.types.iter().any(|x| x.name == t.name) {
                    catalog.types.push(t.clone());
                }
            }
            for d in &c.domains {
                if !catalog.domains.iter().any(|x| x.name == d.name) {
                    catalog.domains.push(d.clone());
                }
            }
            for f in &c.functions {
                if !catalog
                    .functions
                    .iter()
                    .any(|x| x.name == f.name && x.args == f.args)
                {
                    catalog.functions.push(f.clone());
                }
            }
            for k in &c.collations {
                if !catalog.collations.iter().any(|x| x.name == k.name) {
                    catalog.collations.push(k.clone());
                }
            }
        }
        Ok((catalog, at))
    }

    /// The snails, in the order they were opened.
    pub fn snails(&self) -> &[Snail] {
        &self.snails
    }

    /// Every snail's catalog as one.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The generation of the root each snail reads.
    pub fn generations(&self) -> Vec<u64> {
        self.snails.iter().map(Snail::generation).collect()
    }

    /// The pages read from every snail's files.
    pub fn pages_read(&self) -> u64 {
        self.snails.iter().map(Snail::pages_read).sum()
    }

    /// The pages found held, rather than read from the files.
    pub fn pages_held(&self) -> u64 {
        self.snails.iter().map(Snail::pages_held).sum()
    }

    /// The pages the opens read: each snail's root's two copies and its catalog.
    pub fn pages_at_open(&self) -> u64 {
        self.snails.iter().map(|s| s.pages_at_open).sum()
    }

    #[doc(hidden)]
    pub fn stop_at(&mut self, stop: Option<StopAcross>) {
        self.stop = stop;
    }

    #[doc(hidden)]
    pub fn between_roots(&mut self, f: Option<Box<dyn FnMut()>>) {
        self.between = f;
    }

    /// The snail that holds the table named so, where one does.
    fn holder(&self, table: &str) -> Option<usize> {
        self.catalog
            .tables
            .iter()
            .position(|t| t.name == table)
            .map(|t| self.at[t].0)
    }

    /// The table a snail is named by in a transaction across snails: the first it holds.
    fn name_of(&self, i: usize) -> Result<String> {
        self.snails[i]
            .catalog
            .tables
            .first()
            .map(|t| t.name.clone())
            .ok_or_else(|| Error::new(format!("{} holds no table", self.paths[i].display())))
    }

    /// The root a decision of snail `decider`'s root, as `decided` holds it, carries for snail
    /// `holder`, whose root stands at `at`: the one it wrote for the generation after, its checksum
    /// held. None where no decision names one.
    fn carried(
        &self,
        decider: usize,
        decided: &Root,
        holder: usize,
        at: u64,
    ) -> Result<Option<Root>> {
        let snails: Vec<&Snail> = self.snails.iter().collect();
        carried_root(
            &snails,
            &|t| self.holder(t),
            &|i| self.paths[i].display().to_string(),
            (decider, decided),
            holder,
            at,
        )
    }

    /// The root a transaction across snails decided for snail `i`, standing at `at`, that is not
    /// yet taken there: carried by another snail's root, as `roots` holds each.
    fn owed(&self, roots: &[&Root], i: usize, at: u64) -> Result<Option<Root>> {
        for (d, root) in roots.iter().enumerate() {
            if d != i {
                if let Some(r) = self.carried(d, root, i, at)? {
                    return Ok(Some(r));
                }
            }
        }
        Ok(None)
    }

    /// Whether every root a decision named has been written, as each snail's newest root says.
    fn landed(&self, d: &Decision) -> Result<bool> {
        for w in &d.roots {
            let Some(i) = self.holder(&w.holds) else {
                return Ok(false);
            };
            let root = &self.snails[i].reader.peek_root()?;
            let written = root.generation > w.generation
                || (root.generation == w.generation
                    && root.crc(self.snails[i].reader.format())? == w.crc);
            if !written {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// The round's moment: every snail's root read, then every root again, until none changed
    /// between, so that each snail stood at the root it took when the first pass ended. Then each
    /// root a decision among them carries, and its snail has not yet taken, is read as written.
    /// The root pages each snail read.
    fn moment(&mut self) -> Result<Vec<u64>> {
        let n = self.snails.len();
        // A transaction of one snapshot took its moment with its first statement.
        if self.snails.iter().all(|s| s.held) {
            for s in &mut self.snails {
                s.rounds += 1;
            }
            return Ok(vec![0; n]);
        }
        let before: Vec<u64> = self.snails.iter().map(Snail::pages_read).collect();
        loop {
            for i in 0..n {
                let s = &mut self.snails[i];
                if s.aborted {
                    return Err(Error::new(super::change::ABORTED));
                }
                if s.begun.is_some() {
                    s.renew_in_transaction()?;
                } else {
                    s.renew()?;
                }
                s.rounds += 1;
                if i == 0 {
                    if let Some(mut f) = self.between.take() {
                        f();
                    }
                }
            }
            if n == 1 {
                break;
            }
            let mut again = false;
            for s in &self.snails {
                again |= s.reader.peek_root()?.generation != s.root.generation;
            }
            if !again {
                break;
            }
        }
        if n > 1 {
            let roots: Vec<Root> = self.snails.iter().map(|s| s.root.clone()).collect();
            let refs: Vec<&Root> = roots.iter().collect();
            for (i, root) in roots.iter().enumerate() {
                if let Some(owed) = self.owed(&refs, i, root.generation)? {
                    self.snails[i].adopt(owed)?;
                }
            }
        }
        Ok(self
            .snails
            .iter()
            .zip(before)
            .map(|(s, b)| s.pages_read() - b)
            .collect())
    }

    /// At the root's turn of every snail, writes each root a decision among them carries that its
    /// snail has not yet taken, and lets go of each pending copy whose deciding root, among these,
    /// never came to carry it. A pending copy decided by a snail not opened with these stays, and
    /// its snail takes no commit.
    fn resolve(&mut self) -> Result<()> {
        let mut newest = Vec::with_capacity(self.snails.len());
        let mut any = false;
        for s in &self.snails {
            newest.push(s.reader.peek_root()?);
            any |= s.reader.pending_copy().is_some();
        }
        for (i, r) in newest.iter().enumerate() {
            any |= newest.iter().enumerate().any(|(d, x)| {
                d != i
                    && x.decided.iter().any(|dec| {
                        dec.roots.iter().any(|w| {
                            w.generation == r.generation + 1 && self.holder(&w.holds) == Some(i)
                        })
                    })
            });
        }
        if !any {
            return Ok(());
        }
        let turns = self.turn_order(&(0..self.snails.len()).collect::<Vec<_>>())?;
        let mut taken = Vec::new();
        let mut done = Ok(());
        for &i in &turns {
            match lock::take_root_turn(self.snails[i].writer.as_ref().unwrap().file()) {
                Ok(()) => taken.push(i),
                Err(e) => {
                    done = Err(e);
                    break;
                }
            }
        }
        if done.is_ok() {
            done = self.resolve_at_turns();
        }
        for &i in taken.iter().rev() {
            lock::give_root_turn(self.snails[i].writer.as_ref().unwrap().file())?;
        }
        done
    }

    fn resolve_at_turns(&mut self) -> Result<()> {
        let mut newest = Vec::with_capacity(self.snails.len());
        let mut copies = Vec::with_capacity(self.snails.len());
        for s in &self.snails {
            newest.push(s.reader.peek_root()?);
            copies.push(s.reader.pending_copy());
        }
        let refs: Vec<&Root> = newest.iter().collect();
        let mut owed = Vec::with_capacity(self.snails.len());
        for (i, r) in newest.iter().enumerate() {
            owed.push(self.owed(&refs, i, r.generation)?);
        }
        for (i, (owed, copy)) in owed.into_iter().zip(copies).enumerate() {
            // A pending copy whose deciding root is among these, and carries no root for it.
            let void = copy.filter(|c| {
                c.pending
                    .as_ref()
                    .is_some_and(|p| self.holder(&p.decider).is_some())
            });
            let s = &mut self.snails[i];
            let (format, parent) = (s.reader.format(), s.reader.root_copy());
            let w = s.writer.as_mut().expect("a snail open to write");
            // A root owed goes over its parent, never over its pending copy.
            match (owed, void) {
                (Some(root), _) => w.write_copy(parent, &root.encode(format)?)?,
                (None, Some(_)) => w.write_copy(1 - parent, &[0u8; PAGE])?,
                (None, None) => {}
            }
        }
        Ok(())
    }

    /// The order snails' root turns are taken in, the same for every writer: by their first
    /// files' places on the machine.
    fn turn_order(&self, of: &[usize]) -> Result<Vec<usize>> {
        let mut keyed = Vec::with_capacity(of.len());
        for &i in of {
            let file = self.snails[i]
                .writer
                .as_ref()
                .expect("a snail open to write")
                .file();
            keyed.push((crate::file::identity(file)?, i));
        }
        keyed.sort();
        Ok(keyed.into_iter().map(|(_, i)| i).collect())
    }

    /// Gives each round across them from now on the room `sizes` names, as `Snail::size_rounds`
    /// gives one snail's.
    pub fn size_rounds(&mut self, sizes: crate::room::Sizes) {
        self.snails[0].reader.room().set_sizes(sizes);
    }

    /// Answers a question on the snails' tables: one round across them.
    pub fn ask(&mut self, sql: &str) -> Result<(RecordBatch, Round)> {
        self.ask_with(sql, &[])
    }

    /// Hands the snails a function of their caller's, which their questions and statements then
    /// call by its name as any function, as `Snail::hand` hands it to one.
    pub fn hand(&mut self, f: super::handed::Function) -> Result<()> {
        super::handed::hand(&mut self.functions, &self.catalog, f)?;
        for s in &mut self.snails {
            s.functions = self.functions.clone();
        }
        Ok(())
    }

    /// Answers a question with its parameters, `$1` and on, given as text: one round across them.
    /// A question on one snail's tables is read as that snail reads it; one on several snails'
    /// tables is read as a join, each table from its own snail.
    pub fn ask_with(&mut self, sql: &str, params: &[String]) -> Result<(RecordBatch, Round)> {
        let sql = &*super::spelled::scanned(sql, self.snails[0].catalog.release)?;
        question::unlocked_words(sql)?;
        super::relations::unread_clauses(sql)?;
        if self.writing {
            self.resolve()?;
        }
        if self.aborted {
            return Err(Error::new(super::change::ABORTED));
        }
        // A question alone, at a session's SERIALIZABLE, is a transaction that ends with it.
        if self.writing && !self.begun && self.snails[0].levels.serializable(false) {
            self.execute_across("BEGIN")?;
            let asked = self.asked(sql, params);
            let end = if asked.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let ended = self.execute_across(end);
            let answer = asked?;
            ended?;
            return Ok(answer);
        }
        let asked = self.asked(sql, params);
        // At SERIALIZABLE a round that fails fails its transaction.
        if asked.is_err() && self.begun && self.snails[0].levels.serializable(true) {
            self.aborted = true;
            for s in &mut self.snails {
                s.aborted = true;
            }
        }
        asked
    }

    /// A transaction of one snapshot's first statement: its one moment on every snail, held for its
    /// life; at SERIALIZABLE, its marks taken on each.
    fn one_snapshot(&mut self) -> Result<()> {
        if !self.begun {
            return Ok(());
        }
        if self.snails[0].levels.serializable(true) {
            if self.snails[0].serial.borrow().is_none() {
                let mut all: Vec<&mut Snail> = self.snails.iter_mut().collect();
                super::serial::snapshot_taken(&mut all, true)?;
                for s in &mut self.snails {
                    s.held = true;
                    s.rounds += 1;
                    s.levels.read();
                }
            }
        } else if self.snails[0].levels.repeatable(true) && !self.snails[0].held {
            self.moment()?;
            for s in &mut self.snails {
                s.held = true;
                s.levels.read();
            }
        }
        Ok(())
    }

    fn asked(&mut self, sql: &str, params: &[String]) -> Result<(RecordBatch, Round)> {
        let _named = match self.snails.as_slice() {
            [one] => crate::oids::named(one.catalog.oids.clone()),
            _ => crate::oids::named_several(),
        };
        if self.snails.len() > 1 && crate::pgcatalog::names_catalog(sql) {
            return Err(Error::new(
                "pg_catalog's tables are read on one snail at a time: each file numbers its own \
                 objects from 16384, so across several their rows would name one OID twice",
            ));
        }
        if self.snails.len() > 1
            && super::sysviews::words_of(sql)
                .iter()
                .any(|w| matches!(w.as_str(), "pg_prepared_statement" | "pg_catalog.pg_prepared_statement"))
        {
            return Err(Error::new(
                "pg_prepared_statement() is read on one snail at a time: each file numbers its own \
                 types from 16384, so across several a parameter's type would name one OID twice",
            ));
        }
        // Its clock is the transaction's across the snails, begun at BEGIN or with the round alone.
        let at = super::change::wall_micros();
        let _clocked = crate::scalars::clocked(
            if self.begun {
                self.snails[0].clock.0
            } else {
                at
            },
            at,
        );
        // The session's settings stand alike on every snail; its zone is the first's.
        let _zoned = self.snails[0].in_zone();
        let _handed = super::handed::holding(&self.functions);
        let _prepared = super::prepared::viewing(&self.session.prepared);
        let room = self.snails[0].reader.room();
        let entered = crate::room::entered(&room);
        if entered.outermost() {
            for s in &self.snails {
                room.hold_anyway(s.own_bytes());
            }
        }
        let mut read = self.snails_named(sql)?;
        if read.is_empty() {
            read.push(0);
        }
        self.one_snapshot()?;
        let at_start = self.moment()?;
        for s in &self.snails {
            s.wasted.borrow_mut().clear();
        }
        // A read in a transaction takes its first snapshot on each snail it reads.
        if self.begun {
            for &i in &read {
                self.snails[i].levels.read();
            }
        }
        let (batch, mut round) = match read.as_slice() {
            [i] => self.snails[*i].answer_with(sql, None, params)?,
            _ => {
                let refs: Vec<&Snail> = self.snails.iter().collect();
                let parsed = question::parse(&self.catalog, sql);
                let first = match &parsed {
                    Ok(q) if params.is_empty() => {
                        let at: Vec<(usize, usize)> =
                            q.sides.iter().map(|s| self.at[s.table]).collect();
                        Some(join::ask_join_across(&refs, &at, q))
                    }
                    _ => None,
                };
                match first {
                    Some(Err(e)) if !join::past_the_room(&e) => return Err(e),
                    Some(Ok(done)) => done,
                    // What the question path does not read, and a join past the round's room, a
                    // pipeline over the snails' relations reads.
                    _ => {
                        let read = parsed;
                        let sql = &*super::spelled::spelled(sql)?;
                        let parsed = super::image::parse_sql(sql)?;
                        let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
                            return Err(read
                                .err()
                                .unwrap_or_else(|| Error::new("a question is one SELECT")));
                        };
                        let q =
                            super::pipeline::windows::written(&super::spelled::between_written(q))?;
                        let q = super::computed(&self.catalog, &q)?.unwrap_or(q);
                        let across = super::relations::Across {
                            snails: refs,
                            at: &self.at,
                            catalog: &self.catalog,
                        };
                        super::relations::answer(&across, &q, params)?
                    }
                }
            }
        };
        if self.snails.len() > 1 && round.snails.is_empty() {
            let mut parts = vec![Round::default(); self.snails.len()];
            if let [i] = read.as_slice() {
                parts[*i] = round.clone();
            }
            round.snails = parts;
        }
        round.pages_at_start = at_start.iter().sum();
        if self.snails.len() > 1 {
            for (part, p) in round.snails.iter_mut().zip(&at_start) {
                part.pages_at_start = *p;
            }
        }
        // A prune is its snail's own commit, and changes no answer.
        for i in 0..self.snails.len() {
            let wasted = self.snails[i].wasted.take();
            if wasted.is_empty() || self.snails[i].reader.pending_copy().is_some() {
                continue;
            }
            let (pruned, written) = self.snails[i].prune(wasted)?;
            round.blocks_pruned += pruned;
            round.pages_written += written;
            if let Some(part) = round.snails.get_mut(i) {
                part.blocks_pruned = pruned;
                part.pages_written = written;
            }
        }
        round.bytes_held = room.most();
        (round.spill_pages_written, round.spill_pages_read) = room.spill_pages();
        Ok((batch, round))
    }

    /// The snails whose tables a question names, in the order they were opened.
    fn snails_named(&self, sql: &str) -> Result<Vec<usize>> {
        let sql = &*super::spelled::spelled(sql)?;
        let parsed = super::image::parse_sql(sql)?;
        let mut names = Vec::new();
        if let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() {
            // A view's tables may lie in any of them.
            if super::relations::names_a_view(&self.catalog, q) {
                return Ok((0..self.snails.len()).collect());
            }
            let mut q = (**q).clone();
            super::each_table(&mut q, &mut |t| {
                if let sqlparser::ast::TableFactor::Table { name, .. } = t {
                    names.push(crate::catalog::object_name(name));
                }
            });
        }
        let mut read: Vec<usize> = names
            .iter()
            .filter_map(|n| {
                let t = self
                    .catalog
                    .tables
                    .iter()
                    .position(|t| t.name == *n)
                    .or_else(|| {
                        self.catalog
                            .tables
                            .iter()
                            .position(|t| t.name.rsplit('.').next() == Some(n.as_str()))
                    })?;
                Some(self.at[t].0)
            })
            .collect();
        read.sort();
        read.dedup();
        Ok(read)
    }

    /// Runs one statement that writes, or begins or ends a transaction across the snails. A
    /// statement writes the snail that holds its table; outside a transaction it commits there. A
    /// transaction's COMMIT commits every snail it wrote, one root deciding.
    /// Its pages read, written and reused are every snail's.
    pub fn execute(&mut self, sql: &str) -> Result<Done> {
        if !self.writing {
            return Err(Error::new(
                "these snails were opened to read; open them to write",
            ));
        }
        let totals = |s: &Snails| {
            s.snails.iter().fold((0, 0, 0), |(r, w, u), x| {
                (r + x.pages_read(), w + x.written(), u + x.reused)
            })
        };
        let before = totals(self);
        let mut done = self.execute_across(sql)?;
        let after = totals(self);
        done.pages_read = after.0 - before.0;
        done.pages_written = after.1 - before.1;
        done.pages_reused = after.2 - before.2;
        Ok(done)
    }

    fn execute_across(&mut self, sql: &str) -> Result<Done> {
        self.resolve()?;
        // WHERE CURRENT OF a cursor finds the row it stands on by the table's key.
        let current;
        let sql = match super::cursor::mentions_current_of(sql) {
            true => {
                current = super::cursor::current_of(&self.catalog, &self.session.cursors, sql)?;
                current.as_str()
            }
            false => sql,
        };
        let st = match statement::parse(&self.catalog, sql) {
            Ok(st) => st,
            Err(e) => {
                if self.begun && !self.aborted {
                    self.failed()?;
                }
                return Err(e);
            }
        };
        if self.aborted
            && !matches!(
                st,
                Statement::Commit
                    | Statement::Rollback
                    | Statement::Session(
                        super::session::Said::RollbackTo { .. }
                            | super::session::Said::Chain { .. }
                    )
            )
        {
            return Err(Error::new(super::change::ABORTED));
        }
        // A statement alone, at a session's SERIALIZABLE, is a transaction that ends with it.
        if !self.begun
            && self.snails[0].levels.serializable(false)
            && matches!(
                st.written(),
                Statement::Insert { .. }
                    | Statement::Update { .. }
                    | Statement::Delete { .. }
                    | Statement::Set { .. }
            )
        {
            self.execute_across("BEGIN")?;
            let done = self.execute_across(sql);
            let end = if done.is_ok() { "COMMIT" } else { "ROLLBACK" };
            let ended = self.execute_across(end);
            let done = done?;
            ended?;
            return Ok(done);
        }
        match st {
            Statement::Begin
            | Statement::Isolation {
                at: statement::Isolated::Begin,
                ..
            } => {
                // In a transaction each snail warns and goes on.
                let begun = self.begun;
                let mut done = Done::default();
                for i in 0..self.snails.len() {
                    done = match begun {
                        true => self.run(i, sql)?,
                        false => self.snails[i].execute(sql)?,
                    };
                }
                if begun {
                    return Ok(done);
                }
                // One clock for the whole transaction, as PostgreSQL's now() is.
                let clock = self.snails[0].clock.0;
                for s in &mut self.snails {
                    s.clock.0 = clock;
                }
                self.begun = true;
                self.written.clear();
                Ok(done)
            }
            Statement::Commit | Statement::Rollback if !self.begun => Ok(Done {
                statement: st.kind(),
                notices: vec!["WARNING:  there is no transaction in progress".into()],
                ..Default::default()
            }),
            Statement::Rollback => {
                self.end_all()?;
                Ok(Done {
                    statement: "ROLLBACK",
                    ..Default::default()
                })
            }
            Statement::Commit => self.commit(),
            Statement::Declare { .. } => Err(Error::new(
                "a table is declared on its own snail: open it alone to write",
            )),
            Statement::Setting { .. } => self.snails[0].execute(sql),
            Statement::Session(said) => self.said_across(said, sql),
            // A question among the statements is asked across them, its answer in what it did.
            Statement::Question { .. } => {
                let (batch, _) = self.ask(sql)?;
                Ok(Done {
                    statement: "SELECT",
                    rows: batch.num_rows() as u64,
                    answer: Some(batch),
                    ..Default::default()
                })
            }
            // Each snail holds the transaction's level, and refuses a change after its first read.
            Statement::Isolation { .. } => {
                let mut done = Done::default();
                for i in 0..self.snails.len() {
                    done = self.run(i, sql)?;
                }
                Ok(done)
            }
            Statement::Constraints { names, deferred } => {
                // Each snail takes the names it finds, as its own SET CONSTRAINTS finds them; a name
                // none finds is refused by the snail naming its schema, or the first.
                let mut done = Done::default();
                let Some(names) = names else {
                    for i in 0..self.snails.len() {
                        done = self.run(i, sql)?;
                    }
                    return Ok(done);
                };
                let mut given: Vec<Vec<String>> = vec![Vec::new(); self.snails.len()];
                for n in &names {
                    let found: Vec<usize> = (0..self.snails.len())
                        .filter(|&i| super::foreign::named(&self.snails[i].catalog, n).is_ok())
                        .collect();
                    if found.is_empty() {
                        let schema = n.rsplit_once('.').map(|(s, _)| s);
                        let i = (0..self.snails.len())
                            .find(|&i| {
                                schema.is_some_and(|s| self.snails[i].catalog.holds_schema(s))
                            })
                            .unwrap_or(0);
                        return self.run(i, &set_constraints(std::slice::from_ref(n), deferred));
                    }
                    for i in found {
                        given[i].push(n.clone());
                    }
                }
                for (i, ns) in given.iter().enumerate() {
                    if !ns.is_empty() {
                        done = self.run(i, &set_constraints(ns, deferred))?;
                    }
                }
                Ok(done)
            }
            Statement::Writes { .. } => Err(Error::new(
                "a WITH holding writes is written on one snail, not across several",
            )),
            st @ (Statement::Insert { .. }
            | Statement::Update { .. }
            | Statement::Delete { .. }
            | Statement::Set { .. }
            | Statement::Returning { .. }
            | Statement::OnConflict { .. }
            | Statement::Checked { .. }) => {
                // A write and its RETURNING run on the snail holding its table, which reads both.
                let (table, source) = match st.written() {
                    Statement::Insert { table, source } => (*table, Some(source)),
                    Statement::Update { table, .. }
                    | Statement::Delete { table, .. }
                    | Statement::Set { table, .. } => (*table, None),
                    _ => unreachable!(),
                };
                let (i, _) = self.at[table];
                // The rows an INSERT … SELECT takes are read on the snail it writes.
                if let Some(statement::Source::Select { question, .. }) = source {
                    if self.snails_named(question)?.iter().any(|&j| j != i) {
                        return Err(Error::new(format!(
                            "a statement across snails reads only the snail it writes: {} lies in \
                             {}, and its SELECT reads another snail's table",
                            self.catalog.tables[table].name,
                            self.paths[i].display()
                        )));
                    }
                }
                if let Err(e) = self.one_snapshot() {
                    self.aborted = self.begun;
                    for s in &mut self.snails {
                        s.aborted = self.aborted;
                    }
                    return Err(e);
                }
                let done = self.run(i, sql)?;
                if self.begun && !self.written.contains(&i) {
                    self.written.push(i);
                }
                Ok(done)
            }
        }
    }

    /// A statement failed in the transaction across them: on every snail, as each fails one, what was
    /// done since its newest savepoint let go, or, with none, the transaction takes only its end.
    fn failed(&mut self) -> Result<()> {
        for s in &mut self.snails {
            if !s.aborted {
                s.failed()?;
            }
            s.aborted = true;
        }
        self.aborted = true;
        if let Some((_, w, cursors)) = self.savepoints.last() {
            self.written = w.clone();
            let cursors = cursors.clone();
            self.session.cursors.retain(|c| cursors.contains(&c.id));
        }
        Ok(())
    }

    /// A statement of the session's own across them: a savepoint's on every snail, a chained end,
    /// LOCK TABLE on the snail holding each table.
    fn said_across(&mut self, said: super::session::Said, sql: &str) -> Result<Done> {
        use super::session::Said;
        let outside =
            |what: &str| Error::new(format!("{what} can only be used in transaction blocks"));
        let every = |me: &mut Snails| -> Result<Done> {
            let mut done = Done::default();
            for s in &mut me.snails {
                done = s.execute(sql)?;
            }
            Ok(done)
        };
        let named = |me: &mut Snails, name: &str| -> Result<usize> {
            me.savepoints
                .iter()
                .rposition(|(n, _, _)| n == name)
                .ok_or_else(|| Error::new(format!("savepoint \"{name}\" does not exist")))
        };
        let ran = match said {
            Said::Savepoint { name } => {
                if !self.begun {
                    return Err(outside("SAVEPOINT"));
                }
                let done = every(self);
                if done.is_ok() {
                    let cursors = self.session.cursors.iter().map(|c| c.id).collect();
                    self.savepoints.push((name, self.written.clone(), cursors));
                }
                done
            }
            Said::RollbackTo { name } => {
                if !self.begun {
                    return Err(outside("ROLLBACK TO SAVEPOINT"));
                }
                named(self, &name).and_then(|at| {
                    let done = every(self)?;
                    self.savepoints.truncate(at + 1);
                    self.written = self.savepoints[at].1.clone();
                    let cursors = self.savepoints[at].2.clone();
                    self.session.cursors.retain(|c| cursors.contains(&c.id));
                    self.aborted = false;
                    Ok(done)
                })
            }
            Said::Release { name } => {
                if !self.begun {
                    return Err(outside("RELEASE SAVEPOINT"));
                }
                named(self, &name).and_then(|at| {
                    let done = every(self)?;
                    self.savepoints.truncate(at);
                    Ok(done)
                })
            }
            Said::Chain { commit } => {
                let what = if commit { "COMMIT" } else { "ROLLBACK" };
                if !self.begun {
                    return Err(Error::new(format!(
                        "{what} AND CHAIN can only be used in transaction blocks"
                    )));
                }
                let modes = self.snails[0].levels.chained();
                let ended = self.execute_across(what)?;
                let begin = super::statement::begin_text(modes);
                self.execute_across(&begin)?;
                return Ok(ended);
            }
            // The session's settings stand alike on every snail.
            Said::Set { .. } | Said::Reset { .. } => every(self),
            Said::Show { .. } => self.snails[0].execute(sql),
            Said::Prepare {
                name,
                types,
                statement,
            } => super::prepared::prepare(
                std::rc::Rc::make_mut(&mut self.session.prepared),
                &self.catalog,
                &name,
                types,
                &statement,
                (sql, super::change::wall_micros()),
            )
            .map(|_| Done::default()),
            Said::Execute { name, args } => {
                super::prepared::bound(&self.session.prepared, &self.catalog, &name, &args)
                    .and_then(|text| self.execute_across(&text))
            }
            Said::Deallocate { name } => {
                super::prepared::deallocate(
                    std::rc::Rc::make_mut(&mut self.session.prepared),
                    name.as_deref(),
                )
                .map(|_| Done::default())
            }
            Said::Declare(d) => {
                let mut cursors = std::mem::take(&mut self.session.cursors);
                let mut count = self.session.cursor_count;
                let catalog = self.catalog.clone();
                let begun = self.begun;
                let declared = super::cursor::declare(
                    &mut cursors,
                    &mut count,
                    &catalog,
                    begun,
                    d,
                    None,
                    &mut |q| self.ask(q).map(|(b, _)| b),
                );
                self.session.cursors = cursors;
                self.session.cursor_count = count;
                declared.map(|_| Done::default())
            }
            Said::Fetch {
                fetch,
                direction,
                name,
            } => super::cursor::fetch_from(&mut self.session.cursors, fetch, &direction, &name),
            Said::Close { name } => super::cursor::close(&mut self.session.cursors, name.as_deref())
                .map(|_| Done::default()),
            Said::Copy(c) => {
                let catalog = self.catalog.clone();
                match c.from_stdin() || !c.to() {
                    true => {
                        let given = self.session.copy_input.take();
                        super::copy::copy_in(&c, &catalog, given.as_deref(), &mut |q| {
                            self.execute_across(q)
                        })
                    }
                    false => super::copy::copy_out(&c, &catalog, &mut |q| {
                        self.ask(q).map(|(b, _)| b)
                    }),
                }
            }
            Said::Explain(e) => super::explain::explain(self, &e),
            Said::Lock {
                tables,
                mode,
                nowait,
            } => {
                if !self.begun {
                    return Err(outside("LOCK TABLE"));
                }
                let tables = super::session::locked_tables(&self.catalog, &tables)?;
                let mut by: std::collections::BTreeMap<usize, Vec<String>> = Default::default();
                for t in tables {
                    by.entry(self.at[t].0)
                        .or_default()
                        .push(self.catalog.tables[t].name.clone());
                }
                let mut done = Done::default();
                for (i, names) in by {
                    let text = format!(
                        "LOCK TABLE {} IN {} MODE{}",
                        names.join(", "),
                        mode.words(),
                        if nowait { " NOWAIT" } else { "" }
                    );
                    done = self.snails[i].execute(&text)?;
                }
                Ok(done)
            }
        };
        if ran.is_err() && self.begun && !self.aborted {
            self.failed()?;
        }
        ran
    }

    /// One statement on one snail; in a transaction, one that fails fails the transaction on every
    /// snail.
    fn run(&mut self, i: usize, sql: &str) -> Result<Done> {
        let done = self.snails[i].execute(sql);
        if done.is_err() && self.begun {
            self.failed()?;
        }
        done
    }

    /// Ends the transaction on every snail, rolled back.
    fn end_all(&mut self) -> Result<()> {
        self.begun = false;
        self.session.end(false);
        self.aborted = false;
        self.written.clear();
        self.savepoints.clear();
        let mut first = Ok(());
        for s in &mut self.snails {
            s.aborted = false;
            if let Some((root, _)) = s.begun.take() {
                s.root = root;
                s.ended(false);
            }
            let r = s.abandon();
            if first.is_ok() {
                first = r;
            }
        }
        first
    }

    /// The transaction's COMMIT: on the one snail it wrote, as that snail commits; on several, one
    /// root deciding.
    fn commit(&mut self) -> Result<Done> {
        if self.aborted {
            self.end_all()?;
            return Ok(Done {
                statement: "ROLLBACK",
                ..Default::default()
            });
        }
        let mut written = std::mem::take(&mut self.written);
        // A prune in the transaction writes its snail too, and a serializable transaction's record each
        // file where another could meet what it read.
        for (i, s) in self.snails.iter().enumerate() {
            let wrote = s.begun.as_ref().is_some_and(|b| b.1);
            if (wrote || s.serial_records()?) && !written.contains(&i) {
                written.push(i);
            }
        }
        if written.len() < 2 {
            let mut done = Done {
                statement: "COMMIT",
                ..Default::default()
            };
            for &i in &written {
                match self.snails[i].execute("COMMIT") {
                    Ok(d) => done = d,
                    Err(e) => {
                        self.end_all()?;
                        return Err(e);
                    }
                }
            }
            self.begun = false;
            self.session.end(true);
            for s in &mut self.snails {
                if s.begun.is_some() {
                    s.execute("COMMIT")?;
                }
            }
            return Ok(done);
        }
        // Each snail's foreign keys deferred to the commit, on its newest root: one that fails
        // rolls the whole transaction back.
        for &i in &written {
            let s = &mut self.snails[i];
            if let Err(e) = s.commit_checked() {
                self.end_all()?;
                return Err(e);
            }
        }
        if let Some(&i) = written
            .iter()
            .find(|&&i| self.snails[i].reader.format() < 9)
        {
            self.end_all()?;
            return Err(Error::new(format!(
                "{} is of format {}, which holds no transaction across snails: `out`, then `write`",
                self.paths[i].display(),
                self.snails[i].reader.format()
            )));
        }
        let names: Vec<String> = written
            .iter()
            .map(|&i| self.name_of(i))
            .collect::<Result<_>>()?;
        let turns = self.turn_order(&written)?;
        let mut writers: Vec<Option<Writer>> =
            self.snails.iter_mut().map(|s| s.writer.take()).collect();
        let mut taken = Vec::new();
        let mut landed = Ok(Vec::new());
        for &i in &turns {
            match lock::take_root_turn(writers[i].as_ref().unwrap().file()) {
                Ok(()) => taken.push(i),
                Err(e) => {
                    landed = Err(e);
                    break;
                }
            }
        }
        if landed.is_ok() {
            landed = written
                .iter()
                .try_for_each(|&i| self.snails[i].serial_tested())
                .and_then(|_| self.land(&written, &names, &mut writers));
        }
        let mut given = Ok(());
        for &i in taken.iter().rev() {
            let g = lock::give_root_turn(writers[i].as_ref().unwrap().file());
            if given.is_ok() {
                given = g;
            }
        }
        for (s, w) in self.snails.iter_mut().zip(writers) {
            s.writer = w;
        }
        let roots = match landed {
            Ok(roots) => roots,
            Err(e) => {
                self.end_all()?;
                return Err(e);
            }
        };
        let mut done = Done {
            statement: "COMMIT",
            ..Default::default()
        };
        for (&i, root) in written.iter().zip(roots) {
            self.snails[i].root = root;
        }
        self.begun = false;
        self.session.end(true);
        for s in &mut self.snails {
            s.begun = None;
            s.abandon()?;
        }
        given?;
        done.generation = self.snails[written[0]].generation();
        Ok(done)
    }

    /// The commit across snails, at every root's turn: each snail's pages; each other snail's new
    /// root as a pending copy; the deciding snail's root, naming the others'; then each of theirs.
    fn land(
        &mut self,
        written: &[usize],
        names: &[String],
        writers: &mut [Option<Writer>],
    ) -> Result<Vec<Root>> {
        let d = written[0];
        let mut roots: Vec<Root> = Vec::with_capacity(written.len());
        let mut bytes: Vec<Vec<u8>> = Vec::with_capacity(written.len());
        // Each root it writes drops what it decided before where every snail that decision named has
        // written its root since.
        let landed_in = |s: &Snails, i: usize| -> Result<Vec<u64>> {
            let mut landed = Vec::new();
            for old in &s.snails[i].reader.peek_root()?.decided {
                if s.landed(old)? {
                    landed.push(old.at);
                }
            }
            Ok(landed)
        };
        // Each snail's root goes into the copies around the one its parent lies in: a pending
        // copy, and the deciding root, into the other; another snail's root, once decided, over its
        // parent, never over its pending copy.
        let mut parents: Vec<u64> = Vec::with_capacity(written.len());
        for &i in &written[1..] {
            let landed = landed_in(self, i)?;
            let w = writers[i].as_mut().unwrap();
            let mut root = self.snails[i].commit_pages(w, &landed)?;
            parents.push(self.snails[i].reader.root_copy());
            bytes.push(w.prepare(&mut root)?);
            roots.push(root);
        }
        // The deciding root carries the others' roots.
        let landed = landed_in(self, d)?;
        self.snails[d].deciding = Some((
            roots
                .iter()
                .zip(&names[1..])
                .zip(&bytes)
                .map(|((r, holds), b)| Written {
                    holds: holds.clone(),
                    generation: r.generation,
                    crc: u32::from_le_bytes(b[b.len() - 4..].try_into().unwrap()),
                })
                .collect(),
            encode_carried(&bytes),
        ));
        let w = writers[d].as_mut().unwrap();
        let mut root = self.snails[d].commit_pages(w, &landed)?;
        let parent = self.snails[d].reader.root_copy();
        let at = root.generation;
        let decision = w.prepare(&mut root)?;
        // Each other snail's root, first as a pending copy beside its root.
        for (k, &i) in written[1..].iter().enumerate() {
            let mut copy = roots[k].clone();
            copy.pending = Some(Pending {
                decider: names[0].clone(),
                at,
            });
            let format = self.snails[i].reader.format();
            let w = writers[i].as_mut().unwrap();
            let wrote = copy
                .encode(format)
                .and_then(|b| w.write_copy(1 - parents[k], &b));
            if let Err(e) = wrote {
                Snails::let_go(&written[1..k + 2], &parents, writers);
                return Err(e);
            }
        }
        if self.stop == Some(StopAcross::BeforeDecision) {
            return Err(Error::new(
                "the commit across snails stopped before the deciding root",
            ));
        }
        if let Err(e) = writers[d]
            .as_mut()
            .unwrap()
            .write_copy(1 - parent, &decision)
        {
            Snails::let_go(&written[1..], &parents, writers);
            return Err(e);
        }
        if self.stop == Some(StopAcross::AfterDecision) {
            return Err(Error::new(
                "the commit across snails stopped after the deciding root",
            ));
        }
        for (k, &i) in written[1..].iter().enumerate() {
            let over = parents[k];
            let w = writers[i].as_mut().unwrap();
            if self.stop == Some(StopAcross::HalfFinal) {
                w.write_copy(over, &bytes[k][..bytes[k].len() / 2])?;
                return Err(Error::new(
                    "the commit across snails stopped halfway through a root after the deciding root",
                ));
            }
            w.write_copy(over, &bytes[k]).map_err(|e| {
                Error::new(format!(
                    "the transaction across snails is committed, and {}'s root was not \
                     written: a round across them writes it ({e})",
                    self.paths[i].display()
                ))
            })?;
        }
        let mut out = Vec::with_capacity(written.len());
        out.push(root);
        out.extend(roots);
        Ok(out)
    }

    /// Lets go of the pending copies a commit across snails wrote before its deciding root, each
    /// beside its snail's root in `parents`.
    fn let_go(of: &[usize], parents: &[u64], writers: &mut [Option<Writer>]) {
        for (k, &i) in of.iter().enumerate() {
            if let Some(w) = writers[i].as_mut() {
                let _ = w.write_copy(1 - parents[k], &[0u8; PAGE]);
            }
        }
    }
}

/// The root a decision of a snail's root, `decided` as the snail at `decided.0` among `snails`
/// holds it, carries for the snail at `holder`, whose root stands at `at`: the one it wrote for the
/// generation after, its checksum held. None where no decision names one. `holds` finds the snail
/// among them holding a table named so; `shown` names a snail in a refusal.
pub(super) fn carried_root(
    snails: &[&Snail],
    holds: &dyn Fn(&str) -> Option<usize>,
    shown: &dyn Fn(usize) -> String,
    decided: (usize, &Root),
    holder: usize,
    at: u64,
) -> Result<Option<Root>> {
    let (decider, decided) = decided;
    for d in &decided.decided {
        for (k, w) in d.roots.iter().enumerate() {
            if w.generation != at + 1 || holds(&w.holds) != Some(holder) {
                continue;
            }
            let bytes = decode_carried(&snails[decider].reader.read_extent(d.carried)?)?;
            let mut page = bytes.get(k).cloned().unwrap_or_default();
            page.resize(PAGE, 0);
            let root = Root::decode(&page)
                .map(|(r, _)| r)
                .filter(|r| r.generation == w.generation);
            let format = snails[holder].reader.format();
            return match root {
                Some(r) if r.crc(format)? == w.crc => Ok(Some(r)),
                _ => Err(Error::new(format!(
                    "{} carries a root for {} that is not whole",
                    shown(decider),
                    shown(holder)
                ))),
            };
        }
    }
    Ok(None)
}

/// SET CONSTRAINTS of the names given, each part of each quoted, deferred or immediate.
fn set_constraints(names: &[String], deferred: bool) -> String {
    let quoted: Vec<String> = names
        .iter()
        .map(|n| {
            n.split('.')
                .map(|p| format!("\"{}\"", p.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(".")
        })
        .collect();
    let mode = if deferred { "DEFERRED" } else { "IMMEDIATE" };
    format!("SET CONSTRAINTS {} {mode}", quoted.join(", "))
}

impl Snail {
    /// Reads from now on as of a pending copy of the root a round across snails found decided.
    pub(super) fn adopt(&mut self, mut root: Root) -> Result<()> {
        root.pending = None;
        self.reader.adopt(&root);
        let root = self.reader.expanded(root)?;
        if self.begun.is_some() {
            return self.in_transaction_on(root);
        }
        if self.catalog_moved(&root) {
            self.catalog = Snail::catalog(&self.reader, &root)?;
        }
        self.root = root;
        Ok(())
    }
}

impl super::explain::Runs for Snails {
    fn ask_round(&mut self, sql: &str) -> Result<(RecordBatch, Round)> {
        self.ask(sql)
    }
    fn run(&mut self, sql: &str) -> Result<Done> {
        self.execute(sql)
    }
    /// A question priced from the directory of the one snail holding every table it names.
    fn priced(&mut self, sql: &str) -> Result<super::Price> {
        match self.snails_named(sql)?.as_slice() {
            [i] => self.snails[*i].price(sql, None),
            _ => Err(Error::new(
                "a question over several snails' tables is priced only as it runs",
            )),
        }
    }
}

impl Snails {
    /// Runs COPY … FROM STDIN with the rows `input` holds, as a client's COPY data hands them.
    pub fn execute_copy(&mut self, sql: &str, input: &[u8]) -> Result<Done> {
        self.session.copy_input = Some(input.to_vec());
        let done = self.execute(sql);
        self.session.copy_input = None;
        done
    }
}
