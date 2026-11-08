//! WITH RECURSIVE: a query that reads itself, made round by round as PostgreSQL makes it. Its
//! non-recursive term is made once and is the first working table; each round makes the recursive
//! term over the working table, and the rows it adds, every row under UNION ALL and under UNION
//! only those not yet made, are the next; the query is every row made, once a round adds none.

use super::{arm_of, carried_met, carried_of, coerce, in_types, met, Arm, Made, Making};
use super::{Step, Untyped};
use crate::catalog::{Catalog, ColumnType};
use crate::read::collate::Carried;
use crate::read::pipeline::{self, Relation, Value};
use crate::read::spill::{Rows, Run};
use crate::{Error, Result};
use sqlparser::ast::{
    Cte, JoinOperator, LimitClause, ObjectName, ObjectNamePart, Query, SetExpr, SetOperator,
    SetQuantifier, TableFactor, TableWithJoins,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};
use std::collections::HashSet;
use std::ops::Range;

/// The most rows a recursion makes before it is refused: a row group's, as the plain Parquet
/// library cuts one.
const MOST_ROWS: usize = 1 << 20;

/// A recursive query's two terms: the non-recursive term, an arm made once, and the recursive
/// term, an arm made each round from its own steps, by their places among the statement's.
pub(in crate::read) struct Recursion {
    pub(super) left: Arm,
    pub(super) right: Arm,
    all: bool,
    pub(super) body: Range<usize>,
}

/// A WITH RECURSIVE's queries in the order they are made, each after those it reads, each with
/// whether it reads itself; refused as PostgreSQL refuses two queries that read each other.
pub(super) fn ordered(ctes: &[Cte]) -> Result<Vec<(&Cte, bool)>> {
    let names: Vec<String> = ctes
        .iter()
        .map(|c| crate::catalog::ident(&c.alias.name))
        .collect();
    for (i, n) in names.iter().enumerate() {
        if names[..i].contains(n) {
            return Err(Error::new(format!(
                "WITH query name \"{n}\" specified more than once"
            )));
        }
    }
    let mut items: Vec<(usize, HashSet<usize>, bool)> = Vec::with_capacity(ctes.len());
    for (i, c) in ctes.iter().enumerate() {
        let read: HashSet<String> = named(&c.query.to_string())?
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let on = (0..ctes.len())
            .filter(|&j| j != i && read.contains(&names[j]))
            .collect();
        items.push((i, on, read.contains(&names[i])));
    }
    // PostgreSQL's TopologicalSort: each place takes the first item left that reads none left.
    for i in 0..items.len() {
        let Some(j) = (i..items.len()).find(|&j| items[j].1.is_empty()) else {
            return Err(Error::new(
                "mutual recursion between WITH items is not implemented",
            ));
        };
        items.swap(i, j);
        let done = items[i].0;
        for item in &mut items[i + 1..] {
            item.1.remove(&done);
        }
    }
    Ok(items.into_iter().map(|(i, _, r)| (&ctes[i], r)).collect())
}

/// A recursive query as steps: its non-recursive term's, then its recursive term's, which the
/// recursion makes in each round, then the recursion itself under `name`. Refused, before any row,
/// where PostgreSQL's parser refuses its form.
pub(super) fn lift(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    name: String,
    query: Query,
    renames: Vec<String>,
) -> Result<()> {
    let shown = super::apart::written(&name).to_string();
    let mut query = query;
    // The query's own WITH, written before its terms or before the parentheses around them: both
    // terms read it. A WITH on each side of the parentheses is refused before this (`apart`).
    let mut own = query.with.take();
    while let SetExpr::Query(inner) = query.body.as_ref() {
        if query.order_by.is_some()
            || query.limit_clause.is_some()
            || query.fetch.is_some()
            || !query.locks.is_empty()
        {
            break;
        }
        query = (**inner).clone();
        if let Some(w) = query.with.take() {
            own = Some(w);
        }
    }
    let SetExpr::SetOperation {
        left,
        op: SetOperator::Union,
        set_quantifier,
        right,
    } = query.body.as_ref()
    else {
        return Err(Error::new(format!(
            "recursive query \"{shown}\" does not have the form non-recursive-term UNION [ALL] \
             recursive-term"
        )));
    };
    if let Some(w) = &own {
        for c in &w.cte_tables {
            if named(&c.query.to_string())?.iter().any(|(n, _)| *n == name) {
                return Err(Error::new(format!(
                    "recursive reference to query \"{shown}\" must not appear within a subquery"
                )));
            }
        }
    }
    if query.order_by.is_some() {
        return Err(Error::new(
            "ORDER BY in a recursive query is not implemented",
        ));
    }
    let (limit, offset) = match &query.limit_clause {
        Some(LimitClause::LimitOffset { limit, offset, .. }) => (limit.is_some(), offset.is_some()),
        Some(LimitClause::OffsetCommaLimit { .. }) => (true, true),
        None => (false, false),
    };
    if offset {
        return Err(Error::new("OFFSET in a recursive query is not implemented"));
    }
    if limit || query.fetch.is_some() {
        return Err(Error::new("LIMIT in a recursive query is not implemented"));
    }
    if !query.locks.is_empty() {
        return Err(Error::new(
            "FOR UPDATE/SHARE in a recursive query is not implemented",
        ));
    }
    let all = match set_quantifier {
        SetQuantifier::All => true,
        SetQuantifier::Distinct | SetQuantifier::None => false,
        q => return Err(Error::new(format!("UNION {q} is not read"))),
    };
    if named(&left.to_string())?.iter().any(|(n, _)| *n == name) {
        return Err(Error::new(format!(
            "recursive reference to query \"{shown}\" must not appear within its non-recursive term"
        )));
    }
    let mut walk = Walk {
        name: &name,
        found: 0,
        aggregated: false,
    };
    walk.body(right, Within::Term)?;
    if named(&right.to_string())?
        .iter()
        .any(|(n, sub)| *sub && *n == name)
    {
        return Err(Error::new(format!(
            "recursive reference to query \"{shown}\" must not appear within a subquery"
        )));
    }
    if walk.aggregated {
        return Err(Error::new(
            "aggregate functions are not allowed in a recursive query's recursive term",
        ));
    }
    // The query's own WITH, made once before both terms.
    let mut theirs = visible.to_vec();
    if let Some(w) = &own {
        if w.cte_tables.iter().any(|c| super::writes(&c.query)) {
            return Err(Error::new(
                "WITH clause containing a data-modifying statement must be at the top level",
            ));
        }
        super::with_steps(catalog, out, &mut theirs, w)?;
    }
    let left = arm_of(catalog, out, &theirs, left)?;
    let mut seen = theirs;
    seen.push(name.clone());
    let start = out.len();
    let right = arm_of(catalog, out, &seen, right)?;
    let body = start..out.len();
    out.push(Step {
        name,
        query,
        renames,
        calls: Vec::new(),
        set: None,
        laterals: Vec::new(),
        recursion: Some(Recursion {
            left,
            right,
            all,
            body,
        }),
    });
    Ok(())
}

/// Where a recursive term names its query, as PostgreSQL's checkWellFormedRecursion tells them.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Within {
    Term,
    OuterJoin,
    Intersect,
    Except,
}

/// A recursive term walked for the places it names its query in.
struct Walk<'n> {
    name: &'n str,
    found: usize,
    /// Whether a SELECT naming the query in its own FROM calls an aggregate.
    aggregated: bool,
}

impl Walk<'_> {
    fn body(&mut self, b: &SetExpr, at: Within) -> Result<()> {
        match b {
            SetExpr::Select(s) => {
                let mut direct = false;
                for item in &s.from {
                    direct |= self.item(item, at)?;
                }
                self.aggregated |= direct && pipeline::aggregates(s);
                Ok(())
            }
            SetExpr::Query(q) => self.body(&q.body, at),
            SetExpr::SetOperation {
                op,
                set_quantifier,
                left,
                right,
            } => {
                let all = matches!(set_quantifier, SetQuantifier::All);
                let (l, r) = match (at, op) {
                    (Within::Term, SetOperator::Intersect) if all => {
                        (Within::Intersect, Within::Intersect)
                    }
                    (Within::Term, SetOperator::Except) if all => (Within::Except, Within::Except),
                    (Within::Term, SetOperator::Except) => (Within::Term, Within::Except),
                    _ => (at, at),
                };
                self.body(left, l)?;
                self.body(right, r)
            }
            _ => Ok(()),
        }
    }

    /// A FROM item, each relation of it on the side of its joins it lies on; whether it names the
    /// query itself, outside any derived table.
    fn item(&mut self, item: &TableWithJoins, at: Within) -> Result<bool> {
        let mut within = vec![at; item.joins.len() + 1];
        if at == Within::Term {
            for (k, j) in item.joins.iter().enumerate() {
                match &j.join_operator {
                    JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => {
                        within[k + 1] = Within::OuterJoin
                    }
                    JoinOperator::Right(_) | JoinOperator::RightOuter(_) => {
                        within[..=k].fill(Within::OuterJoin)
                    }
                    JoinOperator::FullOuter(_) => within[..=k + 1].fill(Within::OuterJoin),
                    _ => {}
                }
            }
        }
        let mut direct = false;
        for (k, t) in std::iter::once(&item.relation)
            .chain(item.joins.iter().map(|j| &j.relation))
            .enumerate()
        {
            direct |= self.factor(t, within[k])?;
        }
        Ok(direct)
    }

    fn factor(&mut self, t: &TableFactor, at: Within) -> Result<bool> {
        match t {
            TableFactor::Table { name, .. } if bare(name).as_deref() == Some(self.name) => {
                let place = match at {
                    Within::Term => None,
                    Within::OuterJoin => Some("an outer join"),
                    Within::Intersect => Some("INTERSECT"),
                    Within::Except => Some("EXCEPT"),
                };
                if let Some(place) = place {
                    return Err(Error::new(format!(
                        "recursive reference to query \"{}\" must not appear within {place}",
                        super::apart::written(self.name)
                    )));
                }
                self.found += 1;
                if self.found > 1 {
                    return Err(Error::new(format!(
                        "recursive reference to query \"{}\" must not appear more than once",
                        super::apart::written(self.name)
                    )));
                }
                Ok(true)
            }
            TableFactor::Derived { subquery, .. } => {
                self.body(&subquery.body, at)?;
                Ok(false)
            }
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => self.item(table_with_joins, at),
            _ => Ok(false),
        }
    }
}

/// A relation's name where it is one bare name.
fn bare(name: &ObjectName) -> Option<String> {
    match name.0.as_slice() {
        [ObjectNamePart::Identifier(i)] => Some(crate::catalog::ident(i)),
        _ => None,
    }
}

/// Each relation a query's text names by one bare name in a FROM, with whether the name lies
/// inside a subquery of an expression rather than in a FROM's own relations.
fn named(text: &str) -> Result<Vec<(String, bool)>> {
    let tokens: Vec<Token> = Tokenizer::new(&PostgreSqlDialect {}, text)
        .tokenize()?
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let word = |t: Option<&Token>| match t {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_lowercase()),
        _ => None,
    };
    // Each parenthesis open: the clause it is in, whether it lies in an expression's subquery, and
    // whether a query stands where it opens.
    let mut levels: Vec<(String, bool, bool)> = vec![(String::new(), false, true)];
    let mut out = Vec::new();
    for (i, t) in tokens.iter().enumerate() {
        let prev = i.checked_sub(1).and_then(|j| tokens.get(j));
        let (clause, sub, _) = levels.last().cloned().unwrap_or_default();
        match t {
            Token::LParen => {
                let query = matches!(
                    word(tokens.get(i + 1)).as_deref(),
                    Some("select" | "with" | "values")
                );
                let before = word(prev);
                let operator = |w: Option<String>| {
                    matches!(w.as_deref(), Some("union" | "intersect" | "except"))
                };
                let placed = clause == "from"
                    && (matches!(prev, Some(Token::Comma | Token::LParen))
                        || matches!(before.as_deref(), Some("from" | "join" | "lateral")));
                let stands = prev.is_none()
                    || placed
                    || operator(before.clone())
                    || (matches!(before.as_deref(), Some("all" | "distinct"))
                        && operator(word(i.checked_sub(2).and_then(|j| tokens.get(j)))))
                    || (clause == "with"
                        && matches!(before.as_deref(), Some("as" | "materialized")))
                    || (matches!(prev, Some(Token::LParen)) && levels.last().is_some_and(|l| l.2));
                let inner = match placed && !query {
                    true => "from".to_string(),
                    false => String::new(),
                };
                levels.push((inner, sub || (query && !stands), stands));
            }
            Token::RParen => {
                if levels.len() > 1 {
                    levels.pop();
                }
            }
            Token::Word(w) => {
                let lower = w.value.to_lowercase();
                if w.quote_style.is_none()
                    && matches!(
                        lower.as_str(),
                        "select"
                            | "from"
                            | "where"
                            | "group"
                            | "having"
                            | "window"
                            | "order"
                            | "limit"
                            | "offset"
                            | "fetch"
                            | "union"
                            | "intersect"
                            | "except"
                            | "with"
                            | "values"
                            | "qualify"
                    )
                {
                    if let Some(l) = levels.last_mut() {
                        l.0 = lower;
                    }
                    continue;
                }
                let after = matches!(word(prev).as_deref(), Some("from" | "join"))
                    || (clause == "from" && matches!(prev, Some(Token::Comma)));
                let qualified = matches!(tokens.get(i + 1), Some(Token::Period));
                if after && !qualified {
                    let n = match w.quote_style {
                        Some(_) => w.value.clone(),
                        None => lower,
                    };
                    out.push((n, sub));
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// A statement's text refused where a recursive query carries a SEARCH or CYCLE clause, which the
/// snail does not read; PostgreSQL's words where the query is not recursive.
pub(in crate::read) fn unread_clauses(sql: &str) -> Result<()> {
    let lower = sql.to_ascii_lowercase();
    if !lower.contains("with") || !(lower.contains("search") || lower.contains("cycle")) {
        return Ok(());
    }
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return Ok(());
    };
    let tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let word = |t: Option<&Token>| match t {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_lowercase()),
        _ => None,
    };
    // Each parenthesis open: whether it holds a WITH query, and whether that WITH is recursive.
    let mut open: Vec<Option<bool>> = Vec::new();
    let mut with: Vec<Option<bool>> = vec![None];
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::LParen => {
                let prev = word(i.checked_sub(1).and_then(|j| tokens.get(j)));
                let held = match with.last().copied().flatten() {
                    Some(r) if matches!(prev.as_deref(), Some("as" | "materialized")) => Some(r),
                    _ => None,
                };
                open.push(held);
                with.push(None);
            }
            Token::RParen => {
                with.pop();
                if let Some(Some(recursive)) = open.pop() {
                    let clause = match word(tokens.get(i + 1)).as_deref() {
                        Some("search") => "SEARCH",
                        Some("cycle") => "CYCLE",
                        _ => continue,
                    };
                    if !recursive {
                        return Err(Error::new("WITH query is not recursive"));
                    }
                    return Err(Error::new(format!(
                        "a recursive query's {clause} clause is not read: write the column it \
                         adds in the query's terms"
                    )));
                }
            }
            Token::Word(w) if w.quote_style.is_none() => match w.value.to_lowercase().as_str() {
                "with" => {
                    let recursive = word(tokens.get(i + 1)).as_deref() == Some("recursive");
                    if let Some(l) = with.last_mut() {
                        *l = Some(recursive);
                    }
                }
                "select" | "values" | "insert" | "update" | "delete" => {
                    if let Some(l) = with.last_mut() {
                        *l = None;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok(())
}

/// A recursive query made: its non-recursive term, then round by round its recursive term over
/// the rows the round before added, until a round adds none.
pub(in crate::read) fn make(
    making: &mut Making<'_, '_>,
    step: &Step,
    before: &mut [Step],
) -> Result<Made> {
    let rec = step
        .recursion
        .as_ref()
        .ok_or_else(|| Error::new("a recursion's terms were not lifted"))?;
    let name = &step.name;
    let shown = super::apart::written(name);
    let seed = met(&rec.left, &making.made, &making.casings)?;
    let n = seed.types.len();
    // Each column typed as the non-recursive term types it, an untyped literal or NULL as text.
    let types: Vec<ColumnType> = (0..n)
        .map(|c| match (&seed.untyped[c], seed.types[c]) {
            (Untyped::No, Some(t)) => t,
            _ => ColumnType::Text(None),
        })
        .collect();
    let carried: Vec<Carried> = (0..n)
        .map(|c| match &seed.carried[c] {
            Carried::None => carried_of(Some(types[c])),
            k => k.clone(),
        })
        .collect();
    if !rec.all {
        if let Some(&ty) = types.iter().find(|t| !t.grouped()) {
            return Err(Error::new(format!(
                "could not identify an equality operator for type {}",
                ty.unequal_part().unwrap_or(ty).pg_name()
            )));
        }
        if types.iter().any(|t| t.hash_opclass().is_none()) {
            return Err(Error::new(
                "could not implement recursive UNION\nDETAIL:  All column datatypes must be \
                 hashable.",
            ));
        }
    }
    let mut names = seed.names.clone();
    if step.renames.len() > names.len() {
        return Err(Error::new(format!(
            "WITH query \"{shown}\" has {} columns available but {} columns specified",
            names.len(),
            step.renames.len()
        )));
    }
    for (n, r) in names.iter_mut().zip(&step.renames) {
        *n = r.clone();
    }
    let under = carried
        .iter()
        .map(|k| making.casings.carried_under(k))
        .collect::<Result<Vec<_>>>()?;
    let mut seen: HashSet<Vec<pipeline::Key>> = HashSet::new();
    let mut seen_held = crate::room::holding(0);
    // Each row of a term in the recursion's types, where it is new: a place of its own.
    let mut fresh = |rows: &Rows, from: &[Option<ColumnType>], untyped: &[Untyped]| -> Result<Run> {
        let run = Run::new();
        rows.each_chunk(&mut |c| {
            for mut row in c {
                in_types(&mut row, from, untyped, &types)?;
                if !rec.all {
                    let k = pipeline::row_key(&row, &under)?;
                    if seen.contains(&k) {
                        continue;
                    }
                    seen_held.more(crate::read::spill::row_width(&row));
                    seen.insert(k);
                }
                run.push(row)?;
            }
            Ok(())
        })?;
        run.seal()?;
        Ok(run)
    };
    let mut working = fresh(&seed.rows, &seed.types, &seed.untyped)?;
    let mut total = working.len();
    // Every run's working table one after another, none copied.
    let mut all = Rows::default();
    loop {
        let run = Rows::one(working);
        all = all.then(run.clone());
        let mut table = Made::of(
            Relation {
                names: names.clone(),
                rows: Vec::new(),
                order: None,
            },
            types.iter().map(|&t| Some(t)).collect(),
            carried.clone(),
        );
        table.kept = Some(run);
        making.made.insert(name.clone(), std::rc::Rc::new(table));
        for s in &mut before[rec.body.clone()] {
            making.make(s)?;
        }
        let r = met(&rec.right, &making.made, &making.casings)?;
        if r.types.len() != n {
            return Err(Error::new(
                "each UNION query must have the same number of columns",
            ));
        }
        for c in 0..n {
            let left = match seed.untyped[c] {
                Untyped::No => seed.types[c],
                _ => None,
            };
            let right = match r.untyped[c] {
                Untyped::No => r.types[c],
                _ => None,
            };
            let overall = crate::read::typed::common_type(&[left, right], "UNION")?;
            // A literal is read in the type found before any row, as PostgreSQL's parser reads it.
            if let Untyped::Literal(s) = &r.untyped[c] {
                coerce(Value::Text(s.clone()), overall, true)?;
            }
            if overall != types[c] {
                return Err(Error::new(format!(
                    "recursive query \"{shown}\" column {} has type {} in non-recursive term but \
                     type {} overall",
                    c + 1,
                    types[c].pg_name(),
                    overall.pg_name()
                )));
            }
            let collatable = crate::read::collate::collatable(types[c]);
            if let (true, Carried::Named(k)) = (
                collatable,
                carried_met(&carried[c], &r.carried[c], rec.all)?,
            ) {
                if carried[c] != Carried::Named(k.clone()) {
                    let declared = match &carried[c] {
                        Carried::Named(d) => d.clone(),
                        _ => crate::read::collate::DEFAULT.to_string(),
                    };
                    return Err(Error::new(format!(
                        "recursive query \"{shown}\" column {} has collation \"{declared}\" in \
                         non-recursive term but collation \"{k}\" overall",
                        c + 1
                    )));
                }
            }
        }
        let added = fresh(&r.rows, &r.types, &r.untyped)?;
        if added.len() == 0 {
            break;
        }
        total += added.len();
        if total > MOST_ROWS {
            return Err(Error::new(format!(
                "recursive query \"{shown}\" made more than {MOST_ROWS} rows: a recursion is read \
                 only where it ends within a row group's rows"
            )));
        }
        working = added;
    }
    let mut made = Made::of(
        Relation {
            names,
            rows: Vec::new(),
            order: None,
        },
        types.into_iter().map(Some).collect(),
        carried,
    );
    made.kept = Some(all);
    Ok(made)
}

