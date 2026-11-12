//! Cursors, as PostgreSQL's DECLARE opens them: a question's answer as its declaration read it, the
//! generation it read then, FETCH and MOVE taking rows from where the cursor stands, and an UPDATE
//! or DELETE … WHERE CURRENT OF reaching the row it stands on by the table's key, or, where it has
//! none, one row holding the values that row holds.

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
    /// key there, by the table's key columns, or every column it stores where it has no key.
    answer: RecordBatch,
    keys: Option<(usize, Vec<usize>, RecordBatch)>,
    /// Where its question is read a block at a time: a snail holding the generation the declaration
    /// read, the round under way on it, and its question; none once every block has come.
    lazy: Option<Box<(Snail, super::Scan, String)>>,
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
pub struct Declaration {
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

    /// The rows a direction needs read before it is taken: those up to a place, or every one.
    fn needs(&self, d: &Direction) -> Option<usize> {
        match d {
            Direction::Next | Direction::First => Some(self.at + 1),
            Direction::Prior | Direction::Backward(_) => Some(self.at),
            Direction::Absolute(k) if *k >= 0 => Some(*k as usize),
            Direction::Relative(k) if *k >= 0 => Some(self.at + *k as usize),
            Direction::Relative(_) => Some(self.at),
            Direction::Forward(Some(k)) if *k >= 0 => Some(self.at + *k as usize),
            Direction::Forward(Some(_)) => Some(self.at),
            _ => None,
        }
    }

    /// Its round read on, a block at a time, until it holds `rows` rows or every block has come.
    fn read_up_to(&mut self, rows: Option<usize>) -> Result<u64> {
        let mut pages = 0;
        while self.lazy.is_some() && rows.is_none_or(|n| self.rows() < n) {
            let (snail, scan, question) = &mut **self.lazy.as_mut().unwrap();
            let before = snail.pages_read();
            let block = snail.next_block(scan);
            pages += snail.pages_read() - before;
            match block? {
                Some(b) if b.num_rows() > 0 => {
                    let (answer, keys) = match &self.keys {
                        Some((_, key, _)) => {
                            let width = b.num_columns() - key.len();
                            (
                                b.project(&(0..width).collect::<Vec<_>>())?,
                                Some(b.project(&(width..b.num_columns()).collect::<Vec<_>>())?),
                            )
                        }
                        None => (b, None),
                    };
                    self.answer = match self.answer.num_columns() {
                        0 => answer,
                        _ => arrow::compute::concat_batches(&self.answer.schema(), [&self.answer, &answer])?,
                    };
                    if let (Some((_, _, held)), Some(k)) = (self.keys.as_mut(), keys) {
                        *held = match held.num_columns() {
                            0 => k,
                            _ => arrow::compute::concat_batches(&held.schema(), [&*held, &k])?,
                        };
                    }
                }
                Some(_) => {}
                None => {
                    // An answer of no rows takes its columns from the question read whole.
                    if self.answer.num_columns() == 0 {
                        let b = snail.ask(question, None)?.0;
                        let width = b.num_columns() - self.keys.as_ref().map_or(0, |k| k.1.len());
                        self.answer = b.project(&(0..width).collect::<Vec<_>>())?;
                    }
                    self.lazy = None;
                }
            }
        }
        Ok(pages)
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

    /// DECLARE: the question answered now, on the generation the statement reads, and kept as the
    /// cursor's rows.
    pub(super) fn declare(
        list: &mut Vec<Cursor>,
        count: &mut u64,
        catalog: &crate::catalog::Catalog,
        begun: bool,
        d: Declaration,
        lazy: Option<&std::path::Path>,
        ask: &mut dyn FnMut(&str) -> Result<RecordBatch>,
    ) -> Result<()> {
        if !begun && !d.hold {
            return Err(Error::new(
                "DECLARE CURSOR can only be used in transaction blocks",
            ));
        }
        if list.iter().any(|c| c.name == d.name) {
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
        let keyed = cursor_keys(catalog, &d.question);
        // Where the round can stop early, a block of the table at a time along its order, the
        // cursor reads only the blocks FETCH needs, on a snail of its own holding the generation
        // the declaration read.
        let asked = keyed.as_ref().map_or(d.question.clone(), |k| k.2.clone());
        let read_lazily = lazy
            .filter(|_| !locking)
            .map(|path| -> Result<Option<(Snail, super::Scan)>> {
                let snail = Snail::open(path)?;
                Ok(match snail.scan(&asked, None) {
                    Ok(scan) if !scan.whole => Some((snail, scan)),
                    _ => None,
                })
            })
            .transpose()?
            .flatten();
        if let Some((snail, scan)) = read_lazily {
            let empty = RecordBatch::new_empty(std::sync::Arc::new(arrow::datatypes::Schema::empty()));
            *count += 1;
            list.push(Cursor {
                id: *count,
                name: d.name,
                hold: d.hold,
                fresh: begun,
                no_scroll: d.scroll == Some(false),
                answer: empty.clone(),
                keys: keyed.map(|(t, k, _)| (t, k, empty)),
                lazy: Some(Box::new((snail, scan, asked))),
                at: 0,
            });
            return Ok(());
        }
        let (answer, keys) = match keyed {
            Some((table, key, asked)) => {
                let batch = ask(&asked)?;
                let width = batch.num_columns() - key.len();
                let answer = batch.project(&(0..width).collect::<Vec<_>>())?;
                let keys = batch.project(&(width..batch.num_columns()).collect::<Vec<_>>())?;
                (answer, Some((table, key, keys)))
            }
            None => (ask(&d.question)?, None),
        };
        *count += 1;
        list.push(Cursor {
            id: *count,
            name: d.name,
            hold: d.hold,
            fresh: begun,
            no_scroll: d.scroll == Some(false),
            answer,
            keys,
            lazy: None,
            at: 0,
        });
        Ok(())
    }

    /// The table a cursor's question reads alone, with no grouping, its key's columns, and the
    /// question with those columns after its own; none for any other question.
    fn cursor_keys(catalog: &crate::catalog::Catalog, question: &str) -> Option<(usize, Vec<usize>, String)> {
        use sqlparser::ast::{SelectItem, SetExpr, Statement as Sql, TableFactor};
        let parsed = super::image::parse_sql(question).ok()?;
        let [Sql::Query(q)] = parsed.as_slice() else {
            return None;
        };
        let SetExpr::Select(s) = q.body.as_ref() else {
            return None;
        };
        // An aggregate of its own makes one group of its rows, as GROUP BY does.
        let aggregates = s.projection.iter().any(|p| match p {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                calls_aggregate(e)
            }
            _ => false,
        }) || matches!(
            q.order_by.as_ref().map(|o| &o.kind),
            Some(sqlparser::ast::OrderByKind::Expressions(os))
                if os.iter().any(|o| calls_aggregate(&o.expr))
        );
        if s.from.len() != 1
            || !s.from[0].joins.is_empty()
            || super::pipeline::says_distinct(s)
            || s.having.is_some()
            || !matches!(&s.group_by, sqlparser::ast::GroupByExpr::Expressions(g, _) if g.is_empty())
            || aggregates
            || q.with.is_some()
        {
            return None;
        }
        let TableFactor::Table { name, alias, .. } = &s.from[0].relation else {
            return None;
        };
        let n = crate::catalog::object_name(name);
        let ti = catalog.tables.iter().position(|t| t.name == n)?;
        let table = &catalog.tables[ti];
        let (key, _) = key_columns(table);
        let by = alias
            .as_ref()
            .map(|a| crate::catalog::ident(&a.name))
            .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
        // Each named apart from the question's own columns, a `*`'s among them, so that ORDER BY
        // reads a name as the question gives it.
        let mut q2 = (**q).clone();
        if let SetExpr::Select(s2) = q2.body.as_mut() {
            for (j, &c) in key.iter().enumerate() {
                let col = &table.columns[c].name;
                let text = format!(
                    "SELECT \"{}\".\"{}\" AS \"?key{j}\"",
                    by.replace('"', "\"\""),
                    col.replace('"', "\"\"")
                );
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
    pub(super) fn fetch_from(list: &mut [Cursor], fetch: bool, d: &Direction, name: &str) -> Result<super::Done> {
        let c = list
            .iter_mut()
            .find(|c| c.name == name)
            .ok_or_else(|| Error::new(format!("cursor \"{name}\" does not exist")))?;
        let need = c.needs(d);
        let read = c.read_up_to(need)?;
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
            pages_read: read,
            rows: n,
            answer,
            ..Default::default()
        })
    }

    /// CLOSE: the cursor let go, or every one.
    pub(super) fn close(list: &mut Vec<Cursor>, name: Option<&str>) -> Result<()> {
        match name {
            None => list.clear(),
            Some(n) => {
                let at = list
                    .iter()
                    .position(|c| c.name == n)
                    .ok_or_else(|| Error::new(format!("cursor \"{n}\" does not exist")))?;
                list.remove(at);
            }
        }
        Ok(())
    }

    /// `WHERE CURRENT OF name` in a statement written as the condition on the table's key that finds
    /// the row the cursor stands on; the statement as it is where it names no cursor.
    pub(super) fn current_of(
        catalog: &crate::catalog::Catalog,
        list: &[Cursor],
        sql: &str,
    ) -> Result<String> {
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
        let c = list
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
        let table = &catalog.tables[*ti];
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
        let condition = match key_columns(table).1 {
            true => {
                for (k, &col) in key.iter().enumerate() {
                    let v =
                        arrow::util::display::array_value_to_string(keys.column(k).as_ref(), r)?;
                    conds.push(format!(
                        "\"{}\" = '{}'",
                        table.columns[col].name.replace('"', "\"\""),
                        v.replace('\'', "''")
                    ));
                }
                conds.join(" AND ")
            }
            // With no unique key the row is one of those holding its values, each written as
            // PostgreSQL writes it and read back, one of no B-tree class compared as text: the
            // statement changes one of them, as it would the row the cursor stands on.
            false => {
                let texts = super::answer_text(&keys.slice(r, 1))?;
                for (&col, v) in key.iter().zip(&texts[0]) {
                    let column = &table.columns[col];
                    let name = super::relations::quote(&column.name);
                    conds.push(match (v, column.ty.ordered()) {
                        (None, _) => format!("{name} IS NULL"),
                        (Some(v), true) => format!("{name} = '{}'", v.replace('\'', "''")),
                        (Some(v), false) => format!("{name}::text = '{}'", v.replace('\'', "''")),
                    });
                }
                format!(
                    "ctid = (SELECT ctid FROM {} WHERE {} LIMIT 1)",
                    super::relations::quote_name(&table.name),
                    conds.join(" AND ")
                )
            }
        };
        let start = words[at].1;
        let end = words.get(at + 2).map_or(sql.len(), |w| w.2);
        Ok(format!("{}{condition}{}", &sql[..start], &sql[end..]))
    }

impl Snail {
    pub(super) fn declare_cursor(&mut self, d: Declaration) -> Result<()> {
        let begun = self.begun.is_some();
        let mut cursors = std::mem::take(&mut self.session.cursors);
        let mut count = self.session.cursor_count;
        let catalog = self.catalog.clone();
        // A transaction that staged nothing reads as the file stands: its cursor may read lazily.
        // At REPEATABLE READ and SERIALIZABLE it reads as the transaction's snapshot does, whole.
        let lazy = (self.rebased.is_empty()
            && self.declared.is_none()
            && !self.held
            && !self.levels.repeatable(begun))
            .then(|| self.reader.path().to_path_buf());
        let declared = declare(&mut cursors, &mut count, &catalog, begun, d, lazy.as_deref(), &mut |q| {
            self.ask(q, None).map(|(b, _)| b)
        });
        self.session.cursors = cursors;
        self.session.cursor_count = count;
        declared
    }

    pub(super) fn fetch_cursor(
        &mut self,
        fetch: bool,
        d: &Direction,
        name: &str,
    ) -> Result<super::Done> {
        fetch_from(&mut self.session.cursors, fetch, d, name)
    }

    pub(super) fn close_cursor(&mut self, name: Option<&str>) -> Result<()> {
        close(&mut self.session.cursors, name)
    }

    pub(super) fn current_of(&self, sql: &str) -> Result<String> {
        current_of(&self.catalog, &self.session.cursors, sql)
    }
}

/// Whether an expression calls an aggregate of its own query, a subquery's apart.
fn calls_aggregate(e: &sqlparser::ast::Expr) -> bool {
    use super::pipeline;
    match e {
        sqlparser::ast::Expr::Function(f)
            if f.over.is_none()
                && (pipeline::is_aggregate(&pipeline::function_name(f))
                    || !f.within_group.is_empty()) =>
        {
            true
        }
        e => {
            let mut parts = pipeline::parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            parts.into_iter().any(calls_aggregate)
        }
    }
}

/// The columns a cursor keeps of each row it reads of a table, for WHERE CURRENT OF: its unique
/// key's, and whether there is one; with none, every column the table stores.
fn key_columns(table: &crate::catalog::Table) -> (Vec<usize>, bool) {
    match unique_key(table) {
        Some(key) => (key, true),
        None => (
            (0..table.columns.len())
                .filter(|&c| !table.columns[c].is_virtual())
                .collect(),
            false,
        ),
    }
}

/// The columns of a table's unique key, its primary key first, each level a column of its own and
/// no row left out of it.
fn unique_key(table: &crate::catalog::Table) -> Option<Vec<usize>> {
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

/// Whether a statement's text holds the words CURRENT OF, in any case, white space between them: a
/// cheap look before its words are read.
pub(super) fn mentions_current_of(sql: &str) -> bool {
    let upper = sql.to_ascii_uppercase();
    upper.match_indices("CURRENT").any(|(at, w)| {
        let rest = upper[at + w.len()..].trim_start();
        rest.len() < upper.len() - at - w.len() && rest.starts_with("OF")
            && !rest[2..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
    })
}
