//! A question that locks the rows it returns: `FOR UPDATE`, `FOR NO KEY UPDATE`, `FOR SHARE` and
//! `FOR KEY SHARE`, with `OF`, `NOWAIT` and `SKIP LOCKED`, read from its words as PostgreSQL's
//! grammar reads them. Each row is locked by two bytes of the file, its key's and the rest's, held
//! shared or alone as the strength takes them, to the transaction's end; the writers of a table
//! whose rows are locked name the rows they change by the same bytes.

use super::change::{rows_of, widen, Waiting};
use super::statement::{Datum, Row};
use super::{Round, Snail};
use crate::catalog::Table;
use crate::lock;
use crate::{Error, Result};
use arrow::array::RecordBatch;
use sqlparser::ast::{
    Expr, JoinOperator, LimitClause, ObjectNamePart, SelectItem, SetExpr, Statement as Sql,
    TableFactor, UnaryOperator, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::Token;
use std::collections::HashSet;

/// How strongly a row is locked, weakest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Strength {
    KeyShare,
    Share,
    NoKeyUpdate,
    Update,
}

impl Strength {
    pub(crate) fn words(self) -> &'static str {
        match self {
            Strength::KeyShare => "FOR KEY SHARE",
            Strength::Share => "FOR SHARE",
            Strength::NoKeyUpdate => "FOR NO KEY UPDATE",
            Strength::Update => "FOR UPDATE",
        }
    }

    /// The bytes of a row's lock this strength takes, of its key's and the rest's, each with
    /// whether it is shared.
    fn bytes(self, (key, rest): (u64, u64)) -> Vec<(u64, bool)> {
        match self {
            Strength::KeyShare => vec![(key, true)],
            Strength::Share => vec![(key, true), (rest, true)],
            Strength::NoKeyUpdate => vec![(rest, false)],
            Strength::Update => vec![(key, false), (rest, false)],
        }
    }
}

/// What a locking clause does where another holds a row: waits, skips the row, or fails; a table
/// several clauses name takes the last of these any names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Waits {
    Wait,
    SkipLocked,
    NoWait,
}

/// One locking clause: its strength, the relations its OF names, and how it waits.
#[derive(Clone, Debug)]
pub(crate) struct Clause {
    pub(crate) strength: Strength,
    pub(crate) of: Vec<String>,
    pub(crate) waits: Waits,
}

/// A question's locking clauses, and the question without them.
#[derive(Clone, Debug)]
pub(crate) struct Locking {
    pub(crate) question: String,
    pub(crate) clauses: Vec<Clause>,
}

/// The locking clauses at the top of a question, read from its words, and the question without
/// them; none where it has none there. A clause inside parentheses is the question's to refuse.
pub(crate) fn clauses(sql: &str) -> Result<Option<Locking>> {
    let lower = sql.to_ascii_lowercase();
    if !lower.contains("for") {
        return Ok(None);
    }
    let Ok(tokens) = super::statement::spans(sql) else {
        return Ok(None);
    };
    // Each word at its depth in parentheses, with where it begins and ends.
    let words: Vec<(usize, Option<String>, usize, usize)> = {
        let mut depth = 0usize;
        let mut out = Vec::new();
        for (t, a, b) in &tokens {
            match t {
                Token::Whitespace(_) => continue,
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth = depth.saturating_sub(1),
                _ => {}
            }
            let word = match t {
                Token::Word(w) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
                Token::Word(w) => Some(format!("\"{}", w.value)),
                Token::Comma => Some(",".into()),
                Token::Period => Some(".".into()),
                Token::SemiColon => Some(";".into()),
                _ => None,
            };
            out.push((depth, word, *a, *b));
        }
        out
    };
    let at = |i: usize, w: &str| {
        words
            .get(i)
            .is_some_and(|x| x.0 == 0 && x.1.as_deref() == Some(w))
    };
    let mut clauses = Vec::new();
    let mut cut: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < words.len() {
        if !at(i, "FOR") {
            i += 1;
            continue;
        }
        let (strength, after) = if at(i + 1, "UPDATE") {
            (Strength::Update, i + 2)
        } else if at(i + 1, "NO") && at(i + 2, "KEY") && at(i + 3, "UPDATE") {
            (Strength::NoKeyUpdate, i + 4)
        } else if at(i + 1, "SHARE") {
            (Strength::Share, i + 2)
        } else if at(i + 1, "KEY") && at(i + 2, "SHARE") {
            (Strength::KeyShare, i + 3)
        } else {
            i += 1;
            continue;
        };
        let mut j = after;
        let mut of = Vec::new();
        if at(j, "OF") {
            j += 1;
            loop {
                let mut name = Vec::new();
                loop {
                    match words.get(j).and_then(|x| x.1.clone()) {
                        Some(w) if w != "," && w != "." && w != ";" => {
                            name.push(match w.strip_prefix('"') {
                                Some(q) => q.to_string(),
                                None => w.to_lowercase(),
                            });
                            j += 1;
                        }
                        _ => break,
                    }
                    if at(j, ".") {
                        j += 1;
                    } else {
                        break;
                    }
                }
                if name.is_empty() {
                    return Err(Error::new(format!(
                        "syntax error at or near the end of {}'s OF",
                        strength.words()
                    )));
                }
                if name.len() > 1 {
                    return Err(Error::new(format!(
                        "{} must specify unqualified relation names",
                        strength.words()
                    )));
                }
                of.push(name.remove(0));
                if at(j, ",") {
                    j += 1;
                } else {
                    break;
                }
            }
        }
        let waits = if at(j, "NOWAIT") {
            j += 1;
            Waits::NoWait
        } else if at(j, "SKIP") && at(j + 1, "LOCKED") {
            j += 2;
            Waits::SkipLocked
        } else {
            Waits::Wait
        };
        cut.push((words[i].2, words[j - 1].3));
        clauses.push(Clause {
            strength,
            of,
            waits,
        });
        i = j;
    }
    if clauses.is_empty() {
        return Ok(None);
    }
    let mut question = String::with_capacity(sql.len());
    let mut from = 0;
    for (a, b) in cut {
        question.push_str(&sql[from..a]);
        from = b;
    }
    question.push_str(&sql[from..]);
    Ok(Some(Locking {
        question: question.trim().trim_end_matches(';').trim_end().to_string(),
        clauses,
    }))
}

/// A table a question locks: its place in the catalog, the name its rows are read by, its name
/// as PostgreSQL's words give it, the strength and the waiting its clauses take, the columns the
/// question reads of it after those it returns, and among them the places of those that know its
/// rows.
struct Locked {
    table: usize,
    name: String,
    strength: Strength,
    waits: Waits,
    read: Vec<usize>,
    key: Vec<usize>,
}

/// What a locking question asks: the question without its limits, every locked table's key after
/// the columns it returns; the columns it returns; its locked tables; and its OFFSET and LIMIT.
struct Plan {
    asked: String,
    width: usize,
    locked: Vec<Locked>,
    offset: u64,
    limit: Option<u64>,
}

/// The columns that know a table's rows: its first unique order of plain NOT NULL columns, or
/// else every column it stores.
pub(crate) fn key_columns(table: &Table) -> Vec<usize> {
    table
        .orders
        .iter()
        .filter(|o| o.unique)
        .find_map(|o| {
            o.levels
                .iter()
                .map(|l| match l {
                    crate::catalog::Level::Column(c) if table.columns[*c].not_null => Some(*c),
                    _ => None,
                })
                .collect::<Option<Vec<usize>>>()
        })
        .unwrap_or_else(|| {
            (0..table.columns.len())
                .filter(|&c| !table.columns[c].is_virtual())
                .collect()
        })
}

/// The bytes a row's key is hashed from, the same in every build of this version.
fn key_bytes(key: &[Datum]) -> Vec<u8> {
    let mut out = Vec::new();
    for d in key {
        match d {
            Datum::Null => out.push(0),
            Datum::Int(v) => {
                out.push(1);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Datum::Text(s) => {
                out.push(2);
                out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
            Datum::Bytes(b) => {
                out.push(3);
                out.extend_from_slice(&(b.len() as u64).to_le_bytes());
                out.extend_from_slice(b);
            }
            Datum::Wide(v) => {
                out.push(4);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Datum::Float4(b) => {
                out.push(5);
                out.extend_from_slice(&b.to_le_bytes());
            }
            Datum::Float8(b) => {
                out.push(6);
                out.extend_from_slice(&b.to_le_bytes());
            }
            other => {
                let s = format!("{other:?}");
                out.push(7);
                out.extend_from_slice(&(s.len() as u64).to_le_bytes());
                out.extend_from_slice(s.as_bytes());
            }
        }
    }
    out
}

/// A number a LIMIT or an OFFSET gives: written out, or a parameter; none for NULL or ALL.
fn count(e: &Expr, params: &[String], what: &str) -> Result<Option<u64>> {
    let text = match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => s.clone(),
            Value::Null => return Ok(None),
            Value::Placeholder(p) => {
                let n: usize = p.trim_start_matches('$').parse().unwrap_or(0);
                params
                    .get(n.wrapping_sub(1))
                    .cloned()
                    .ok_or_else(|| Error::new(format!("there is no parameter {p}")))?
            }
            _ => {
                return Err(Error::new(format!(
                    "{what} {e} is not read beside a locking clause"
                )))
            }
        },
        Expr::Identifier(i) if i.value.eq_ignore_ascii_case("all") => return Ok(None),
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } if matches!(expr.as_ref(), Expr::Value(v) if matches!(v.value, Value::Number(..))) => {
            format!("-{expr}")
        }
        _ => {
            return Err(Error::new(format!(
                "{what} {e} is not read beside a locking clause"
            )))
        }
    };
    let n: i64 = text
        .trim()
        .parse()
        .map_err(|_| Error::new(format!("invalid input syntax for type bigint: \"{text}\"")))?;
    if n < 0 {
        return Err(Error::new(format!("{what} must not be negative")));
    }
    Ok(Some(n as u64))
}

impl Snail {
    /// The plan of a locking question, refused in PostgreSQL's words where PostgreSQL refuses it.
    fn lock_plan(&self, l: &Locking, params: &[String], whole: bool) -> Result<Plan> {
        let first = l.clauses[0].strength.words();
        let bound = crate::grammar::bound(&l.question);
        let parsed = super::image::parse_sql(bound.as_deref().unwrap_or(&l.question))?;
        let mut q = match parsed.as_slice() {
            [Sql::Query(q)] => (**q).clone(),
            _ => return Err(Error::new(format!("{first} is read only on a SELECT"))),
        };
        if q.with.is_some() {
            return Err(Error::new(format!(
                "{first} beside WITH is not read: lock the rows of a question on tables"
            )));
        }
        if q.fetch.is_some() {
            return Err(Error::new(format!("FETCH is not read beside {first}")));
        }
        let s = match q.body.as_mut() {
            SetExpr::Select(s) => s,
            SetExpr::SetOperation { .. } => {
                return Err(Error::new(format!(
                    "{first} is not allowed with UNION/INTERSECT/EXCEPT"
                )))
            }
            _ => return Err(Error::new(format!("{first} is read only on a SELECT"))),
        };
        // FROM's tables are looked up before the locking clause is read, as PostgreSQL reads them.
        for item in &s.from {
            let sides = std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation));
            for f in sides {
                if let TableFactor::Table { name, .. } = f {
                    let n = crate::catalog::object_name(name);
                    if !self.catalog.tables.iter().any(|t| t.name == n) {
                        return Err(Error::new(format!("relation \"{n}\" does not exist")));
                    }
                }
            }
        }
        if s.distinct.is_some() {
            return Err(Error::new(format!(
                "{first} is not allowed with DISTINCT clause"
            )));
        }
        let grouped = !matches!(
            &s.group_by,
            sqlparser::ast::GroupByExpr::Expressions(e, m) if e.is_empty() && m.is_empty()
        );
        if grouped {
            return Err(Error::new(format!(
                "{first} is not allowed with GROUP BY clause"
            )));
        }
        if s.having.is_some() {
            return Err(Error::new(format!(
                "{first} is not allowed with HAVING clause"
            )));
        }
        let items: Vec<&Expr> = s
            .projection
            .iter()
            .filter_map(|p| match p {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
                _ => None,
            })
            .collect();
        let calls: Vec<(bool, bool, bool)> =
            items.iter().map(|e| super::pipeline::calls_in(e)).collect();
        if calls.iter().any(|c| c.0) {
            return Err(Error::new(format!(
                "{first} is not allowed with aggregate functions"
            )));
        }
        if calls.iter().any(|c| c.1) {
            return Err(Error::new(format!(
                "{first} is not allowed with window functions"
            )));
        }
        if calls.iter().any(|c| c.2) {
            return Err(Error::new(format!(
                "{first} is not allowed with set-returning functions in the target list"
            )));
        }
        // The relations of its FROM: each table, the name its rows are read by, and whether an
        // outer join makes its side nullable.
        let mut relations: Vec<(usize, String, bool)> = Vec::new();
        for item in &s.from {
            let mut sides = vec![(&item.relation, false)];
            for j in &item.joins {
                let (left_nullable, right_nullable) = match &j.join_operator {
                    JoinOperator::Left(_) | JoinOperator::LeftOuter(_) => (false, true),
                    JoinOperator::Right(_) | JoinOperator::RightOuter(_) => (true, false),
                    JoinOperator::FullOuter(_) => (true, true),
                    _ => (false, false),
                };
                if left_nullable {
                    for s in &mut sides {
                        s.1 = true;
                    }
                }
                sides.push((&j.relation, right_nullable));
            }
            for (f, nullable) in sides {
                let TableFactor::Table { name, alias, .. } = f else {
                    return Err(Error::new(format!("{first} over {f} is not read")));
                };
                let n = crate::catalog::object_name(name);
                let ti = self
                    .catalog
                    .tables
                    .iter()
                    .position(|t| t.name == n)
                    .ok_or_else(|| Error::new(format!("relation \"{n}\" does not exist")))?;
                let by = match alias {
                    Some(a) => crate::catalog::ident(&a.name),
                    None => match name.0.last() {
                        Some(ObjectNamePart::Identifier(i)) => crate::catalog::ident(i),
                        _ => n.clone(),
                    },
                };
                relations.push((ti, by, nullable));
            }
        }
        if relations.is_empty() {
            return Err(Error::new(format!("{first} reads the rows of a table")));
        }
        let mut locked: Vec<Locked> = Vec::new();
        for c in &l.clauses {
            for o in &c.of {
                if !relations.iter().any(|(_, by, _)| by == o) {
                    return Err(Error::new(format!(
                        "relation \"{o}\" in {} clause not found in FROM clause",
                        c.strength.words()
                    )));
                }
            }
        }
        for (ti, by, nullable) in &relations {
            let named: Vec<&Clause> = l
                .clauses
                .iter()
                .filter(|c| c.of.is_empty() || c.of.contains(by))
                .collect();
            let Some(strength) = named.iter().map(|c| c.strength).max() else {
                continue;
            };
            if *nullable {
                return Err(Error::new(format!(
                    "{} cannot be applied to the nullable side of an outer join",
                    strength.words()
                )));
            }
            let table = &self.catalog.tables[*ti];
            // Each locked relation's key is read after the columns the question returns; at
            // REPEATABLE READ every column it stores, so that a row changed since is known.
            let key = key_columns(table);
            let read: Vec<usize> = match whole {
                true => (0..table.columns.len())
                    .filter(|&c| !table.columns[c].is_virtual())
                    .collect(),
                false => key.clone(),
            };
            locked.push(Locked {
                table: *ti,
                name: table
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap_or(&table.name)
                    .to_string(),
                strength,
                waits: named.iter().map(|c| c.waits).max().unwrap_or(Waits::Wait),
                key: key
                    .iter()
                    .map(|c| read.iter().position(|r| r == c).expect("a key column read"))
                    .collect(),
                read,
            });
            let k = locked.len() - 1;
            for (j, &c) in locked[k].read.iter().enumerate() {
                let item = format!(
                    "\"{}\".\"{}\" AS \"?lock{k}.{j}\"",
                    by.replace('"', "\"\""),
                    table.columns[c].name.replace('"', "\"\"")
                );
                let e = sqlparser::parser::Parser::new(&PostgreSqlDialect {})
                    .try_with_sql(&item)?
                    .parse_select_item()?;
                s.projection.push(e);
            }
        }
        let width = s.projection.len() - locked.iter().map(|t| t.read.len()).sum::<usize>();
        let (mut offset, mut limit) = (0, None);
        match q.limit_clause.take() {
            None => {}
            Some(LimitClause::LimitOffset {
                limit: l,
                offset: o,
                limit_by,
            }) if limit_by.is_empty() => {
                if let Some(e) = &l {
                    limit = count(e, params, "LIMIT")?;
                }
                if let Some(o) = &o {
                    offset = count(&o.value, params, "OFFSET")?.unwrap_or(0);
                }
            }
            Some(other) => return Err(Error::new(format!("{other} is not read beside {first}"))),
        }
        Ok(Plan {
            asked: q.to_string(),
            width,
            locked,
            offset,
            limit,
        })
    }

    /// Answers a question with locking clauses, as PostgreSQL's READ COMMITTED does: its rows found
    /// as the round began, each locked in the strength its clauses take, waiting for, skipping or
    /// failing on a row another transaction holds; where a commit landed meanwhile, found again on
    /// the newest root, kept to the rows first found, in their newest versions. The locks are held
    /// to the transaction's end, or the statement's where it stands alone.
    pub(super) fn ask_locking(
        &mut self,
        l: Locking,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        if self.writer.is_none() {
            return Err(Error::new(format!(
                "{} is not read on a snail opened to read: open it to write, and its rows are \
                 locked",
                l.clauses[0].strength.words()
            )));
        }
        let answered = match self.levels.read_only(self.begun.is_some()) {
            true => Err(Error::new(format!(
                "cannot execute SELECT {} in a read-only transaction",
                l.clauses[0].strength.words()
            ))),
            false => self.locked_answer(&l, order, params),
        };
        if answered.is_err() && self.begun.is_some() {
            self.failed()?;
        } else if self.begun.is_none() {
            self.let_go_blocks()?;
        }
        answered
    }

    fn locked_answer(
        &mut self,
        l: &Locking,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let repeatable = self.levels.repeatable(self.begun.is_some());
        let plan = self.lock_plan(l, params, repeatable)?;
        let file = self
            .writer
            .as_ref()
            .expect("a snail open to write")
            .file()
            .try_clone()?;
        for t in &plan.locked {
            self.lock_table(t.table, super::tablelock::Mode::RowShare, false)?;
        }
        for t in &plan.locked {
            self.take_bytes(&[(lock::lockers_byte(t.table), true)], Waiting::Try)?;
        }
        let mut found: Option<HashSet<Vec<Datum>>> = None;
        'round: loop {
            let base = self.root.generation;
            let (batch, round) = self.answer_with(&plan.asked, order, params)?;
            let rows: Vec<Row> =
                rows_of(&batch.project(&(plan.width..batch.num_columns()).collect::<Vec<_>>())?)?;
            let kept: Vec<usize> = match &found {
                None => {
                    found = Some(rows.iter().cloned().collect());
                    (0..rows.len()).collect()
                }
                Some(first) => (0..rows.len())
                    .filter(|&r| first.contains(&rows[r]))
                    .collect(),
            };
            // A table a writer changed rows of without naming them: its rows wait for that writer.
            let mut skipped = vec![false; plan.locked.len()];
            for (k, t) in plan.locked.iter().enumerate() {
                let unnamed = lock::unnamed_byte(t.table);
                if !lock::held_by_other(&file, unnamed)? {
                    continue;
                }
                match t.waits {
                    Waits::NoWait => return Err(not_obtained(&t.name)),
                    Waits::SkipLocked => skipped[k] = true,
                    Waits::Wait => {
                        let own = self.row_locks.get(&unnamed).copied();
                        self.lock_byte_as(unnamed, false, Waiting::Wait)?;
                        self.unlock_byte(unnamed)?;
                        if own.is_some() {
                            self.lock_byte_as(unnamed, true, Waiting::Wait)?;
                        }
                        if !repeatable {
                            self.renew_for_question()?;
                        }
                        continue 'round;
                    }
                }
            }
            let mut chosen = Vec::new();
            let mut locked_rows = Vec::new();
            'rows: for r in kept {
                if plan
                    .limit
                    .is_some_and(|n| locked_rows.len() as u64 >= plan.offset + n)
                {
                    break;
                }
                let mut at = 0;
                for (k, t) in plan.locked.iter().enumerate() {
                    let read = &rows[r][at..at + t.read.len()];
                    at += t.read.len();
                    if skipped[k] {
                        continue 'rows;
                    }
                    let key: Vec<Datum> = t.key.iter().map(|&i| read[i].clone()).collect();
                    let bytes = t.strength.bytes(lock::row_bytes(t.table, &key_bytes(&key)));
                    let waiting = match t.waits {
                        Waits::Wait => Waiting::Wait,
                        _ => Waiting::Try,
                    };
                    if !self.take_bytes(&bytes, waiting)? {
                        match t.waits {
                            Waits::NoWait => return Err(not_obtained(&t.name)),
                            _ => continue 'rows,
                        }
                    }
                }
                locked_rows.push(r);
                if locked_rows.len() as u64 > plan.offset {
                    chosen.push(r as u32);
                }
            }
            // A commit since the round's root: at READ COMMITTED the rows are found again on the
            // newest root; at REPEATABLE READ a row locked that another transaction changed since
            // is refused.
            if self.reader.peek_root()?.generation != base {
                if !repeatable {
                    self.renew_for_question()?;
                    continue;
                }
                self.unchanged_since(&plan, &rows, &locked_rows)?;
            }
            let picked = arrow::array::UInt32Array::from(chosen);
            let columns = (0..plan.width)
                .map(|c| arrow::compute::take(batch.column(c), &picked, None))
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let schema = std::sync::Arc::new(
                batch
                    .schema()
                    .project(&(0..plan.width).collect::<Vec<_>>())?,
            );
            return Ok((RecordBatch::try_new(schema, columns)?, round));
        }
    }

    /// At REPEATABLE READ, refuses the rows a question locked where one of them is not, column for
    /// column, as the newest root holds it: another transaction changed it since the generation the
    /// round read. A row the transaction wrote lies only in the version it reads.
    fn unchanged_since(&mut self, plan: &Plan, rows: &[Row], locked: &[usize]) -> Result<()> {
        let mut at = 0;
        for t in &plan.locked {
            let table = &self.catalog.tables[t.table];
            let own = self.own.get(&table.name);
            let full: Vec<Row> = widen(
                table,
                locked
                    .iter()
                    .map(|&r| rows[r][at..at + t.read.len()].to_vec())
                    .collect(),
            )
            .into_iter()
            .filter(|r| !own.is_some_and(|o| o.holds(r)))
            .collect();
            at += t.read.len();
            let found: Vec<&Row> = full.iter().collect();
            if self.changed_since(t.table, &found)? != super::beside::Since::Same {
                return Err(super::beside::concurrent_update());
            }
        }
        Ok(())
    }

    /// The rows of a parent a foreign key's check found held against a writer that would take their
    /// key, as PostgreSQL's FOR KEY SHARE holds them, to the transaction's end; while the transaction
    /// names at most `NAMED` bytes: past them, its commit checks the key under the parent's block.
    pub(super) fn key_shared(&mut self, ti: usize, rows: &[Row]) -> Result<()> {
        let table = self.catalog.tables[ti].clone();
        let key = key_columns(&table);
        for r in rows {
            if self.row_locks.len() >= NAMED {
                return Ok(());
            }
            let k: Vec<Datum> = key.iter().map(|&c| r[c].clone()).collect();
            self.take_bytes(
                &Strength::KeyShare.bytes(lock::row_bytes(ti, &key_bytes(&k))),
                Waiting::Wait,
            )?;
        }
        Ok(())
    }

    /// The root a question reads again: the newest, in a transaction with its own blocks in place.
    fn renew_for_question(&mut self) -> Result<()> {
        if self.begun.is_some() {
            self.renew_in_transaction()?;
        } else {
            self.renew()?;
        }
        Ok(())
    }

    /// Takes the bytes of one row's lock, each shared or alone, as `waiting` says; a byte held
    /// already in that strength or a stronger one is taken again by nothing. Where one is not
    /// taken, those taken here are let go, and it returns false.
    pub(super) fn take_bytes(&mut self, bytes: &[(u64, bool)], waiting: Waiting) -> Result<bool> {
        let mut got = Vec::new();
        for &(b, shared) in bytes {
            let held = self.row_locks.get(&b).copied();
            if held == Some(false) || (held == Some(true) && shared) {
                continue;
            }
            if !self.lock_byte_as(b, shared, waiting)? {
                for g in got {
                    self.unlock_byte(g)?;
                    self.row_locks.remove(&g);
                }
                return Ok(false);
            }
            if held.is_none() {
                got.push(b);
            }
            self.row_locks.insert(b, shared);
        }
        Ok(true)
    }

    /// The rows a statement changes in table `ti`, each with whether it is deleted or its key
    /// changed: each named by its bytes, waiting for any other that holds one, while the
    /// transaction names at most `NAMED` bytes of rows, or past them where a question has locked
    /// rows of the table; else the table is marked as one whose changed rows are not named, and a
    /// locking question waits for it whole.
    pub(super) fn name_rows(&mut self, ti: usize, rows: &[(&Row, bool)]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let lockers = lock::lockers_byte(ti);
        let unnamed = lock::unnamed_byte(ti);
        let fits = self.row_locks.len() + 2 * rows.len() <= NAMED;
        if !fits {
            // Marked first, so that a question that locks rows of the table from here on finds
            // the mark, or is found by the test below.
            self.take_bytes(&[(unnamed, true)], Waiting::Wait)?;
            let file = self.writer.as_ref().expect("a snail open to write").file();
            if !lock::held_by_other(file, lockers)? && !self.row_locks.contains_key(&lockers) {
                self.unnamed.insert(ti);
                return Ok(());
            }
        }
        let table = self.catalog.tables[ti].clone();
        let key = key_columns(&table);
        for (row, strong) in rows {
            let k: Vec<Datum> = key.iter().map(|&c| row[c].clone()).collect();
            let strength = if *strong {
                Strength::Update
            } else {
                Strength::NoKeyUpdate
            };
            self.take_bytes(
                &strength.bytes(lock::row_bytes(ti, &key_bytes(&k))),
                Waiting::Wait,
            )?;
        }
        if !fits && !self.unnamed.contains(&ti) {
            self.unlock_byte(unnamed)?;
            self.row_locks.remove(&unnamed);
        }
        Ok(())
    }
}

/// The bytes of rows a transaction names, at most, beside a table no question locks rows of.
pub(super) const NAMED: usize = 512;

/// PostgreSQL's words where NOWAIT finds a row locked, SQLSTATE 55P03.
fn not_obtained(relation: &str) -> Error {
    Error::new(format!(
        "could not obtain lock on row in relation \"{relation}\""
    ))
}

/// Whether a row replaced changes a column of one of its table's unique keys: PostgreSQL's key
/// columns, which a write that changes them locks as FOR UPDATE does, where any other takes
/// FOR NO KEY UPDATE.
pub(crate) fn key_changed(table: &Table, old: &Row, new: &Row) -> bool {
    table.orders.iter().filter(|o| o.unique).any(|o| {
        o.levels.iter().any(|l| match l {
            crate::catalog::Level::Column(c) => old[*c] != new[*c],
            _ => false,
        })
    })
}
