//! Cursors, as PostgreSQL's DECLARE opens them: a question's answer as its declaration read it, the
//! generation it read then, FETCH and MOVE taking rows from where the cursor stands, and an UPDATE
//! or DELETE … WHERE CURRENT OF reaching the row it stands on by the table's key.

use super::Snail;
use crate::{Error, Result};
use arrow::array::RecordBatch;

/// A cursor DECLARE opened.
pub(super) struct Cursor {
    /// Its number in the session, which tells it from another of its name declared since.
    pub(super) id: u64,
    pub(super) name: String,
    /// Declared WITH HOLD: it outlives the transaction that commits it.
    pub(super) hold: bool,
    /// Declared in the transaction under way: a rollback closes it, held or not.
    pub(super) fresh: bool,
    /// Declared NO SCROLL: it reads forward alone.
    no_scroll: bool,
    /// Its answer, its rows as the declaration read them; and, for a cursor on one table, each row's
    /// key there, by the table's key columns.
    answer: RecordBatch,
    keys: Option<(usize, Vec<usize>, RecordBatch)>,
    /// Where it stands: 0 before the first row, the rows' count and one after the last.
    at: usize,
}

/// Where FETCH and MOVE take a cursor, as PostgreSQL's grammar reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Direction {
    Next,
    Prior,
    First,
    Last,
    Absolute(i64),
    Relative(i64),
    /// FORWARD n, or n alone; `None` for ALL.
    Forward(Option<i64>),
    /// BACKWARD n; `None` for ALL.
    Backward(Option<i64>),
}

/// DECLARE's words: the cursor's name, BINARY, SCROLL or NO SCROLL, WITH HOLD, and its question.
#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub(super) name: String,
    pub(super) scroll: Option<bool>,
    pub(super) hold: bool,
    pub(super) question: String,
}

/// DECLARE name [BINARY] [ASENSITIVE | INSENSITIVE] [[NO] SCROLL] CURSOR [{WITH | WITHOUT} HOLD]
/// FOR query, read off its words: none where it is no such statement.
pub(super) fn parse_declare(sql: &str) -> Option<Result<Declaration>> {
    use sqlparser::tokenizer::Token;
    let spans = super::statement::spans(sql).ok()?;
    let words: Vec<&(Token, usize, usize)> = spans
        .iter()
        .filter(|(t, _, _)| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let kw = |i: usize| match words.get(i).map(|w| &w.0) {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_uppercase(),
        _ => String::new(),
    };
    if kw(0) != "DECLARE" {
        return None;
    }
    let syntax = |i: usize| {
        Some(Err(Error::new(match words.get(i) {
            Some((t, _, _)) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".into(),
        })))
    };
    let name = match words.get(1).map(|w| &w.0) {
        Some(Token::Word(w)) => match w.quote_style {
            Some(_) => w.value.clone(),
            None => w.value.to_lowercase(),
        },
        _ => return syntax(1),
    };
    let mut i = 2;
    let mut scroll = None;
    loop {
        match kw(i).as_str() {
            "BINARY" | "ASENSITIVE" | "INSENSITIVE" => i += 1,
            "SCROLL" => {
                scroll = Some(true);
                i += 1
            }
            "NO" if kw(i + 1) == "SCROLL" => {
                scroll = Some(false);
                i += 2
            }
            "CURSOR" => {
                i += 1;
                break;
            }
            _ => return syntax(i),
        }
    }
    let mut hold = false;
    match (kw(i).as_str(), kw(i + 1).as_str()) {
        ("WITH", "HOLD") => {
            hold = true;
            i += 2
        }
        ("WITHOUT", "HOLD") => i += 2,
        _ => {}
    }
    if kw(i) != "FOR" {
        return syntax(i);
    }
    let Some(start) = words.get(i + 1).map(|w| w.1) else {
        return syntax(i + 1);
    };
    let question = sql[start..].trim().trim_end_matches(';').trim_end().to_string();
    Some(Ok(Declaration {
        name,
        scroll,
        hold,
        question,
    }))
}

/// FETCH or MOVE [direction] [FROM | IN] name, read off its words: the direction and the cursor's
/// name; none where it is no such statement.
pub(super) fn parse_fetch(
    tokens: &[sqlparser::tokenizer::Token],
) -> Option<Result<(bool, Direction, String)>> {
    use sqlparser::tokenizer::Token;
    let w: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF | Token::SemiColon))
        .collect();
    let kw = |i: usize| match w.get(i) {
        Some(Token::Word(x)) if x.quote_style.is_none() => x.value.to_uppercase(),
        _ => String::new(),
    };
    let fetch = match kw(0).as_str() {
        "FETCH" => true,
        "MOVE" => false,
        _ => return None,
    };
    let syntax = |i: usize| {
        Some(Err(Error::new(match w.get(i) {
            Some(t) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".into(),
        })))
    };
    // A count, signed.
    let count = |i: usize| -> Option<(i64, usize)> {
        let (neg, j) = match w.get(i) {
            Some(Token::Minus) => (true, i + 1),
            Some(Token::Plus) => (false, i + 1),
            _ => (false, i),
        };
        match w.get(j) {
            Some(Token::Number(n, _)) => n.parse::<i64>().ok().map(|v| (if neg { -v } else { v }, j + 1)),
            _ => None,
        }
    };
    let mut i = 1;
    let direction = match kw(i).as_str() {
        "NEXT" => {
            i += 1;
            Direction::Next
        }
        "PRIOR" => {
            i += 1;
            Direction::Prior
        }
        "FIRST" => {
            i += 1;
            Direction::First
        }
        "LAST" => {
            i += 1;
            Direction::Last
        }
        "ABSOLUTE" => match count(i + 1) {
            Some((n, j)) => {
                i = j;
                Direction::Absolute(n)
            }
            None => return syntax(i + 1),
        },
        "RELATIVE" => match count(i + 1) {
            Some((n, j)) => {
                i = j;
                Direction::Relative(n)
            }
            None => return syntax(i + 1),
        },
        "ALL" => {
            i += 1;
            Direction::Forward(None)
        }
        "FORWARD" | "BACKWARD" => {
            let forward = kw(i) == "FORWARD";
            let n = if kw(i + 1) == "ALL" {
                i += 2;
                None
            } else if let Some((n, j)) = count(i + 1) {
                i = j;
                Some(n)
            } else {
                i += 1;
                Some(1)
            };
            match forward {
                true => Direction::Forward(n),
                false => Direction::Backward(n),
            }
        }
        _ => match count(i) {
            Some((n, j)) => {
                i = j;
                Direction::Forward(Some(n))
            }
            None => Direction::Next,
        },
    };
    if matches!(kw(i).as_str(), "FROM" | "IN") {
        i += 1;
    }
    let name = match w.get(i) {
        Some(Token::Word(x)) => match x.quote_style {
            Some(_) => x.value.clone(),
            None => x.value.to_lowercase(),
        },
        _ => return syntax(i),
    };
    if i + 1 != w.len() {
        return syntax(i + 1);
    }
    Some(Ok((fetch, direction, name)))
}

impl Cursor {
    fn rows(&self) -> usize {
        self.answer.num_rows()
    }

    /// The rows a FETCH takes, as their places in the answer in the order it returns them, and the
    /// cursor moved where it then stands.
    fn take(&mut self, d: &Direction) -> Result<Vec<usize>> {
        let n = self.rows();
        let backward = match d {
            Direction::Prior | Direction::Backward(_) => true,
            Direction::First | Direction::Last => false,
            Direction::Absolute(k) | Direction::Relative(k) => {
                matches!(d, Direction::Absolute(_)) && (*k as usize) < self.at || *k < 0
            }
            Direction::Forward(Some(k)) => *k < 0,
            _ => false,
        };
        if backward && self.no_scroll {
            return Err(Error::new("cursor can only scan forward"));
        }
        let clamp = |p: i64| p.clamp(0, n as i64 + 1) as usize;
        let row = |at: usize| (1..=n).contains(&at).then(|| at - 1);
        Ok(match d {
            Direction::Next => {
                self.at = clamp(self.at as i64 + 1);
                row(self.at).into_iter().collect()
            }
            Direction::Prior => {
                self.at = clamp(self.at as i64 - 1);
                row(self.at).into_iter().collect()
            }
            Direction::First => {
                self.at = clamp(1);
                row(self.at).into_iter().collect()
            }
            Direction::Last => {
                self.at = n;
                row(self.at).into_iter().collect()
            }
            Direction::Absolute(k) => {
                self.at = match *k {
                    k if k >= 0 => clamp(k),
                    k => clamp(n as i64 + 1 + k),
                };
                row(self.at).into_iter().collect()
            }
            Direction::Relative(k) => {
                self.at = clamp(self.at as i64 + k);
                row(self.at).into_iter().collect()
            }
            Direction::Forward(Some(0)) | Direction::Backward(Some(0)) => {
                row(self.at).into_iter().collect()
            }
            Direction::Forward(Some(k)) if *k < 0 => self.step(-*k as usize, false),
            Direction::Backward(Some(k)) if *k < 0 => self.step(-*k as usize, true),
            Direction::Forward(k) => self.step(k.map_or(usize::MAX, |k| k as usize), true),
            Direction::Backward(k) => self.step(k.map_or(usize::MAX, |k| k as usize), false),
        })
    }

    /// Up to `k` rows one at a time, forward or backward.
    fn step(&mut self, k: usize, forward: bool) -> Vec<usize> {
        let n = self.rows();
        let mut out = Vec::new();
        while out.len() < k {
            if forward {
                if self.at > n {
                    break;
                }
                self.at += 1;
                if self.at > n {
                    break;
                }
            } else {
                if self.at == 0 {
                    break;
                }
                self.at -= 1;
                if self.at == 0 {
                    break;
                }
            }
            out.push(self.at - 1);
        }
        out
    }
}

impl Snail {
    /// DECLARE: the question answered now, on the generation the statement reads, and kept as the
    /// cursor's rows.
    pub(super) fn declare_cursor(&mut self, d: Declaration) -> Result<()> {
        if self.begun.is_none() && !d.hold {
            return Err(Error::new(
                "DECLARE CURSOR can only be used in transaction blocks",
            ));
        }
        if self.session.cursors.iter().any(|c| c.name == d.name) {
            return Err(Error::new(format!("cursor \"{}\" already exists", d.name)));
        }
        let locking = super::locking::clauses(&d.question)?.is_some();
        if locking && d.hold {
            return Err(Error::new(
                "DECLARE CURSOR WITH HOLD ... FOR UPDATE is not supported",
            ));
        }
        if locking && d.scroll == Some(true) {
            return Err(Error::new(
                "DECLARE SCROLL CURSOR ... FOR UPDATE is not supported",
            ));
        }
        let first = d
            .question
            .split(|c: char| !c.is_ascii_alphabetic())
            .find(|w| !w.is_empty())
            .unwrap_or("")
            .to_ascii_uppercase();
        if !matches!(first.as_str(), "SELECT" | "VALUES" | "WITH" | "TABLE") {
            return Err(Error::new(format!("syntax error at or near \"{first}\"")));
        }
        // A cursor on one table keeps each row's key there, for WHERE CURRENT OF.
        let keyed = self.cursor_keys(&d.question);
        let (answer, keys) = match keyed {
            Some((table, key, asked)) => {
                let batch = self.ask(&asked, None)?.0;
                let width = batch.num_columns() - key.len();
                let answer = batch.project(&(0..width).collect::<Vec<_>>())?;
                let keys = batch.project(&(width..batch.num_columns()).collect::<Vec<_>>())?;
                (answer, Some((table, key, keys)))
            }
            None => (self.ask(&d.question, None)?.0, None),
        };
        self.session.cursor_count += 1;
        self.session.cursors.push(Cursor {
            id: self.session.cursor_count,
            name: d.name,
            hold: d.hold,
            fresh: self.begun.is_some(),
            no_scroll: d.scroll == Some(false),
            answer,
            keys,
            at: 0,
        });
        Ok(())
    }

    /// The table a cursor's question reads alone, with no grouping, its key's columns, and the
    /// question with those columns after its own; none for any other question.
    fn cursor_keys(&self, question: &str) -> Option<(usize, Vec<usize>, String)> {
        use sqlparser::ast::{SelectItem, SetExpr, Statement as Sql, TableFactor};
        let parsed = super::image::parse_sql(question).ok()?;
        let [Sql::Query(q)] = parsed.as_slice() else {
            return None;
        };
        let SetExpr::Select(s) = q.body.as_ref() else {
            return None;
        };
        if s.from.len() != 1
            || !s.from[0].joins.is_empty()
            || s.distinct.is_some()
            || s.having.is_some()
            || !matches!(&s.group_by, sqlparser::ast::GroupByExpr::Expressions(g, _) if g.is_empty())
            || q.with.is_some()
        {
            return None;
        }
        let TableFactor::Table { name, alias, .. } = &s.from[0].relation else {
            return None;
        };
        let n = crate::catalog::object_name(name);
        let ti = self.catalog.tables.iter().position(|t| t.name == n)?;
        let table = &self.catalog.tables[ti];
        let key = key_columns(table)?;
        let by = alias
            .as_ref()
            .map(|a| crate::catalog::ident(&a.name))
            .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
        let mut q2 = (**q).clone();
        if let SetExpr::Select(s2) = q2.body.as_mut() {
            if s2
                .projection
                .iter()
                .any(|p| matches!(p, SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)))
            {
                return None;
            }
            for &c in &key {
                let col = &table.columns[c].name;
                let text = format!("SELECT \"{}\".\"{}\"", by.replace('"', "\"\""), col.replace('"', "\"\""));
                let Ok(p) = super::image::parse_sql(&text) else {
                    return None;
                };
                let Some(Sql::Query(pq)) = p.first() else {
                    return None;
                };
                let SetExpr::Select(ps) = pq.body.as_ref() else {
                    return None;
                };
                s2.projection.push(ps.projection[0].clone());
            }
        }
        Some((ti, key, q2.to_string()))
    }

    /// FETCH or MOVE: the rows taken, as the cursor's answer holds them, and the cursor moved.
    pub(super) fn fetch(&mut self, fetch: bool, d: &Direction, name: &str) -> Result<super::Done> {
        let c = self
            .session
            .cursors
            .iter_mut()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::new(format!("cursor \"{name}\" does not exist")))?;
        let rows = c.take(d)?;
        let n = rows.len() as u64;
        let answer = match fetch {
            true => {
                let idx = arrow::array::UInt32Array::from(
                    rows.iter().map(|&r| r as u32).collect::<Vec<_>>(),
                );
                let columns = c
                    .answer
                    .columns()
                    .iter()
                    .map(|a| arrow::compute::take(a.as_ref(), &idx, None))
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                Some(RecordBatch::try_new_with_options(
                    c.answer.schema(),
                    columns,
                    &arrow::array::RecordBatchOptions::new().with_row_count(Some(rows.len())),
                )?)
            }
            false => None,
        };
        Ok(super::Done {
            statement: if fetch { "FETCH" } else { "MOVE" },
            rows: n,
            answer,
            ..Default::default()
        })
    }

    /// CLOSE: the cursor let go, or every one.
    pub(super) fn close_cursor(&mut self, name: Option<&str>) -> Result<()> {
        match name {
            None => self.session.cursors.clear(),
            Some(n) => {
                let at = self
                    .session
                    .cursors
                    .iter()
                    .position(|c| c.name == n)
                    .ok_or_else(|| Error::new(format!("cursor \"{n}\" does not exist")))?;
                self.session.cursors.remove(at);
            }
        }
        Ok(())
    }

    /// `WHERE CURRENT OF name` in a statement written as the condition on the table's key that finds
    /// the row the cursor stands on; the statement as it is where it names no cursor.
    pub(super) fn current_of(&self, sql: &str) -> Result<String> {
        use sqlparser::tokenizer::Token;
        let spans = super::statement::spans(sql)?;
        let words: Vec<&(Token, usize, usize)> = spans
            .iter()
            .filter(|(t, _, _)| !matches!(t, Token::Whitespace(_) | Token::EOF))
            .collect();
        let kw = |i: usize| match words.get(i).map(|w| &w.0) {
            Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_uppercase(),
            _ => String::new(),
        };
        let Some(at) = (0..words.len()).find(|&i| kw(i) == "CURRENT" && kw(i + 1) == "OF") else {
            return Ok(sql.to_string());
        };
        let name = match words.get(at + 2).map(|w| &w.0) {
            Some(Token::Word(w)) => match w.quote_style {
                Some(_) => w.value.clone(),
                None => w.value.to_lowercase(),
            },
            _ => return Err(Error::new("syntax error at end of input")),
        };
        let c = self
            .session
            .cursors
            .iter()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::new(format!("cursor \"{name}\" does not exist")))?;
        let target = (0..at)
            .find_map(|i| match (kw(i).as_str(), words.get(i + 1).map(|w| &w.0)) {
                ("UPDATE" | "FROM", Some(Token::Word(_))) => {
                    let mut j = i + 1;
                    let mut name = Vec::new();
                    while let Some(Token::Word(w)) = words.get(j).map(|w| &w.0) {
                        name.push(match w.quote_style {
                            Some(_) => w.value.clone(),
                            None => w.value.to_lowercase(),
                        });
                        if matches!(words.get(j + 1).map(|w| &w.0), Some(Token::Period)) {
                            j += 2;
                        } else {
                            break;
                        }
                    }
                    Some(name.join("."))
                }
                _ => None,
            })
            .unwrap_or_default();
        let Some((ti, key, keys)) = &c.keys else {
            return Err(Error::new(format!(
                "cursor \"{name}\" is not a simply updatable scan of table \"{}\"",
                target.rsplit('.').next().unwrap_or_default()
            )));
        };
        let table = &self.catalog.tables[*ti];
        if table.name != target {
            return Err(Error::new(format!(
                "cursor \"{name}\" does not have a FOR UPDATE/SHARE reference to table \"{}\"",
                target.rsplit('.').next().unwrap_or_default()
            )));
        }
        if c.at == 0 || c.at > c.rows() {
            return Err(Error::new(format!(
                "cursor \"{name}\" is not positioned on a row"
            )));
        }
        let r = c.at - 1;
        let mut conds = Vec::new();
        for (k, &col) in key.iter().enumerate() {
            let v = arrow::util::display::array_value_to_string(keys.column(k).as_ref(), r)?;
            conds.push(format!(
                "\"{}\" = '{}'",
                table.columns[col].name.replace('"', "\"\""),
                v.replace('\'', "''")
            ));
        }
        let start = words[at].1;
        let end = words.get(at + 2).map_or(sql.len(), |w| w.2);
        Ok(format!("{}{}{}", &sql[..start], conds.join(" AND "), &sql[end..]))
    }
}

/// The columns of a table's unique key, its primary key first, each level a column of its own and
/// no row left out of it.
fn key_columns(table: &crate::catalog::Table) -> Option<Vec<usize>> {
    let plain = |o: &&crate::catalog::Order| {
        o.unique
            && o.predicate.is_none()
            && o.levels
                .iter()
                .all(|l| matches!(l, crate::catalog::Level::Column(_)))
    };
    let o = table
        .orders
        .iter()
        .filter(plain)
        .find(|o| o.constraint.as_ref().is_some_and(|k| k.primary))
        .or_else(|| table.orders.iter().find(plain))?;
    Some(o.levels.iter().map(|l| l.column()).collect())
}
