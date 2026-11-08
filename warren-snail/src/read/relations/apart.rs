//! A WITH inside a step: each of its queries becomes a step of the question's own, under a name no
//! statement can write. Every WITH below the question's own is given such names before its steps
//! are lifted, and each reference to one of its queries is renamed where PostgreSQL's parser reads
//! the reference as that query: without RECURSIVE a query's name is seen by the queries after it
//! and by the query the WITH stands on, under RECURSIVE by every query of the WITH as well, and a
//! WITH nearer the reference naming a query of the same name hides the farther one. A name renamed
//! keeps the name it was written by as its alias, so a column qualified by it reads the same.

use crate::read::pipeline::MERGED;
use crate::{Error, Result};
use sqlparser::ast::{
    Ident, ObjectName, ObjectNamePart, Query, SetExpr, TableAlias, TableFactor, VisitMut,
    VisitorMut, With,
};
use std::cell::Cell;
use std::ops::ControlFlow;

/// What ends a lifted query's mark in its name: the name the statement wrote follows it.
const WRITTEN: char = '\u{4}';

thread_local! {
    /// The next number a lifted query's name takes, so that no two of a thread's are alike.
    static NEXT: Cell<u64> = const { Cell::new(0) };
}

/// A name of its own for a WITH query written `name`.
fn lifted(name: &str) -> String {
    let n = NEXT.with(|c| c.replace(c.get() + 1));
    format!("{MERGED}with{n}{WRITTEN}{name}")
}

/// A step's name as the statement wrote it: a lifted query's written name, any other step's
/// without its mark.
pub(in crate::read) fn written(name: &str) -> &str {
    match name
        .strip_prefix(MERGED)
        .and_then(|n| n.split_once(WRITTEN))
    {
        Some((_, w)) => w,
        None => name.trim_start_matches(MERGED),
    }
}

/// Every WITH inside `q` given names of its own, `q`'s own WITH kept as written: the question's
/// steps keep the names the question gives them.
pub(super) fn below(q: &mut Query) -> Result<()> {
    one_with(q)?;
    if let Some(w) = &mut q.with {
        for c in &mut w.cte_tables {
            all(&mut c.query)?;
        }
    }
    each_inside(q, &mut |inner| all(inner))
}

/// `q`'s own WITH and every WITH inside it given names of their own, where they have none yet.
pub(super) fn all(q: &mut Query) -> Result<()> {
    if q.with.as_ref().is_some_and(|w| !named_apart(w)) {
        name_apart(q)?;
    }
    below(q)
}

/// Whether a WITH's queries already have names of their own.
fn named_apart(w: &With) -> bool {
    w.cte_tables
        .first()
        .is_some_and(|c| c.alias.name.value.starts_with(MERGED))
}

/// A query in parentheses takes the WITH written before it, as PostgreSQL's grammar puts the
/// clauses written around parentheses on the query inside them; refused where that query has a
/// WITH of its own, in PostgreSQL's words.
fn one_with(q: &Query) -> Result<()> {
    if q.with.is_none() {
        return Ok(());
    }
    let mut body = q.body.as_ref();
    while let SetExpr::Query(inner) = body {
        if inner.with.is_some() {
            return Err(Error::new("multiple WITH clauses not allowed"));
        }
        body = inner.body.as_ref();
    }
    Ok(())
}

/// `q`'s own WITH given names of its own, every reference that reads one of its queries renamed.
fn name_apart(q: &mut Query) -> Result<()> {
    let Some(mut with) = q.with.take() else {
        return Ok(());
    };
    let names: Vec<String> = with
        .cte_tables
        .iter()
        .map(|c| crate::catalog::ident(&c.alias.name))
        .collect();
    for (i, n) in names.iter().enumerate() {
        if names[..i].contains(n) {
            q.with = Some(with);
            return Err(Error::new(format!(
                "WITH query name \"{n}\" specified more than once"
            )));
        }
    }
    let new: Vec<String> = names.iter().map(|n| lifted(n)).collect();
    for (i, (from, to)) in names.iter().zip(&new).enumerate() {
        // Without RECURSIVE a query is seen only by those after it.
        let after = if with.recursive { 0 } else { i + 1 };
        for c in &mut with.cte_tables[after..] {
            rename(&mut c.query, from, to);
        }
        rename(q, from, to);
    }
    for (c, to) in with.cte_tables.iter_mut().zip(&new) {
        c.alias.name = Ident::with_quote('"', to.as_str());
    }
    q.with = Some(with);
    Ok(())
}

/// Each reference `q` makes by a bare name to the relation `from` renamed `to`, where it reads the
/// WITH query of that name: a WITH of `q`'s own, or one inside it, naming a query `from` hides it
/// from the references that see that query instead.
fn rename(q: &mut Query, from: &str, to: &str) {
    if let Some(w) = &mut q.with {
        let at = w
            .cte_tables
            .iter()
            .position(|c| crate::catalog::ident(&c.alias.name) == from);
        let seeing = match (at, w.recursive) {
            (None, _) => w.cte_tables.len(),
            (Some(_), true) => 0,
            // A query does not see its own name without RECURSIVE.
            (Some(i), false) => i + 1,
        };
        for c in &mut w.cte_tables[..seeing] {
            rename(&mut c.query, from, to);
        }
        if at.is_some() {
            return;
        }
    }
    let with = q.with.take();
    let mut r = Renamer { from, to, depth: 0 };
    let _ = q.visit(&mut r);
    q.with = with;
}

/// The references of one query, outside its own WITH: each table in its FROM by the bare name
/// `from` renamed, and each query directly inside it renamed by `rename`, which reads that
/// query's own WITH.
struct Renamer<'n> {
    from: &'n str,
    to: &'n str,
    depth: usize,
}

impl VisitorMut for Renamer<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        self.depth += 1;
        if self.depth == 2 {
            rename(q, self.from, self.to);
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _q: &mut Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, t: &mut TableFactor) -> ControlFlow<()> {
        if self.depth != 1 {
            return ControlFlow::Continue(());
        }
        if let TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } = t
        {
            if let [ObjectNamePart::Identifier(i)] = name.0.as_slice() {
                if crate::catalog::ident(i) == self.from {
                    let by = i.clone();
                    *name = ObjectName(vec![ObjectNamePart::Identifier(Ident::with_quote(
                        '"', self.to,
                    ))]);
                    if alias.is_none() {
                        *alias = Some(TableAlias {
                            explicit: true,
                            name: by,
                            columns: Vec::new(),
                            at: None,
                        });
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }
}

/// `f` on each query directly inside `q`, outside its own WITH: the subqueries of its FROM, its
/// set operation's arms, the subqueries of its expressions; none inside those.
fn each_inside(q: &mut Query, f: &mut dyn FnMut(&mut Query) -> Result<()>) -> Result<()> {
    let with = q.with.take();
    let mut each = Inside {
        depth: 0,
        f,
        failed: None,
    };
    let _ = q.visit(&mut each);
    q.with = with;
    match each.failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

struct Inside<'f> {
    depth: usize,
    f: &'f mut dyn FnMut(&mut Query) -> Result<()>,
    failed: Option<Error>,
}

impl VisitorMut for Inside<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        self.depth += 1;
        if self.depth == 2 {
            if let Err(e) = (self.f)(q) {
                self.failed = Some(e);
                return ControlFlow::Break(());
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _q: &mut Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }
}
