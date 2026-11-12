//! EXPLAIN, as the snail reads it: the snail's round, as `read` prints it. A question on one table
//! is priced from its directory before it runs; with ANALYZE any statement runs, and its round, or a
//! write's pages, are printed as they came. Text, a line a row of `QUERY PLAN`, or JSON.

use super::{Round, Snail};
use crate::catalog::order_shown;
use crate::{Error, Result};

/// EXPLAIN's options, as it names them.
#[derive(Clone, Debug)]
pub struct Explain {
    analyze: bool,
    verbose: bool,
    costs: bool,
    buffers: Option<bool>,
    summary: Option<bool>,
    json: bool,
    statement: String,
}

/// EXPLAIN [ANALYZE] [VERBOSE] statement and EXPLAIN (option [value], …) statement, read off its
/// words: none where the words are no EXPLAIN.
pub(super) fn parse_explain(sql: &str) -> Option<Result<Explain>> {
    use sqlparser::tokenizer::Token;
    let spans = super::statement::spans(sql).ok()?;
    let w: Vec<&(Token, usize, usize)> = spans
        .iter()
        .filter(|(t, _, _)| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    let kw = |i: usize| match w.get(i).map(|x| &x.0) {
        Some(Token::Word(x)) if x.quote_style.is_none() => x.value.to_uppercase(),
        _ => String::new(),
    };
    if kw(0) != "EXPLAIN" {
        return None;
    }
    let syntax = |i: usize| {
        Error::new(match w.get(i) {
            Some((t, _, _)) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".into(),
        })
    };
    Some((|| {
        let mut e = Explain {
            analyze: false,
            verbose: false,
            costs: true,
            buffers: None,
            summary: None,
            json: false,
            statement: String::new(),
        };
        let mut i = 1;
        if matches!(w.get(i).map(|x| &x.0), Some(Token::LParen)) {
            i += 1;
            loop {
                let opt = kw(i).to_lowercase();
                if opt.is_empty() {
                    return Err(syntax(i));
                }
                i += 1;
                let value = match w.get(i).map(|x| &x.0) {
                    Some(Token::Comma) | Some(Token::RParen) => None,
                    Some(Token::Word(x)) => {
                        i += 1;
                        Some(x.value.to_lowercase())
                    }
                    Some(Token::SingleQuotedString(s)) => {
                        i += 1;
                        Some(s.to_lowercase())
                    }
                    Some(Token::Number(n, _)) => {
                        i += 1;
                        Some(n.clone())
                    }
                    _ => return Err(syntax(i)),
                };
                let truth = |v: &Option<String>| -> Result<bool> {
                    match v.as_deref() {
                        None => Ok(true),
                        Some(v) => crate::read::truth(v).ok_or_else(|| {
                            Error::new(format!("{opt} requires a Boolean value"))
                        }),
                    }
                };
                match opt.as_str() {
                    "analyze" => e.analyze = truth(&value)?,
                    "verbose" => e.verbose = truth(&value)?,
                    "costs" => e.costs = truth(&value)?,
                    "buffers" => e.buffers = Some(truth(&value)?),
                    "summary" => e.summary = Some(truth(&value)?),
                    "settings" | "wal" | "timing" | "memory" | "generic_plan" => {
                        truth(&value)?;
                    }
                    "serialize" => match value.as_deref() {
                        None | Some("none" | "text" | "binary") => {}
                        Some(v) => {
                            return Err(Error::new(format!(
                                "unrecognized value for EXPLAIN option \"serialize\": \"{v}\""
                            )))
                        }
                    },
                    "format" => match value.as_deref() {
                        Some("text") => e.json = false,
                        Some("json") => e.json = true,
                        Some(f @ ("xml" | "yaml")) => {
                            return Err(Error::new(format!(
                                "EXPLAIN (FORMAT {}) is refused: the snail prints its round as text \
                                 or JSON",
                                f.to_uppercase()
                            )))
                        }
                        Some(v) => {
                            return Err(Error::new(format!(
                                "unrecognized value for EXPLAIN option \"format\": \"{v}\""
                            )))
                        }
                        None => return Err(Error::new("EXPLAIN option \"format\" requires a value")),
                    },
                    other => {
                        return Err(Error::new(format!(
                            "unrecognized EXPLAIN option \"{other}\""
                        )))
                    }
                }
                match w.get(i).map(|x| &x.0) {
                    Some(Token::Comma) => i += 1,
                    Some(Token::RParen) => {
                        i += 1;
                        break;
                    }
                    _ => return Err(syntax(i)),
                }
            }
        } else {
            if matches!(kw(i).as_str(), "ANALYZE" | "ANALYSE") {
                e.analyze = true;
                i += 1;
            }
            if kw(i) == "VERBOSE" {
                e.verbose = true;
                i += 1;
            }
        }
        let start = w.get(i).map(|x| x.1).ok_or_else(|| syntax(i))?;
        e.statement = sql[start..].trim().trim_end_matches(';').trim_end().to_string();
        Ok(e)
    })())
}

/// A round's lines, as `read` prints it.
fn round_lines(r: &Round, e: &Explain, out: &mut Vec<String>) {
    out.push(format!("Snail round along {}: {} rows", order_shown(&r.order), r.rows));
    if e.costs {
        out.push(format!(
            "  Pages: {} as it began, {} of the directory ({} held), {} of columns predicted, {} of \
             them held and {} read",
            r.pages_at_start,
            r.directory_pages,
            r.directory_pages_held,
            r.column_pages_predicted,
            r.column_pages_held,
            r.column_pages_read
        ));
        out.push(format!(
            "  Entries: {} looked at, {} holding none of its rows, {} some, {} all; {} blocks read",
            r.entries, r.entries_none, r.entries_may, r.entries_must, r.blocks_read
        ));
    }
    if e.verbose {
        for c in &r.consulted {
            out.push(format!(
                "  Order {}: {} pages of its directory; {} to {} column pages by its directory, {}",
                order_shown(&c.order),
                c.directory_pages,
                c.low,
                c.high,
                c.exact.map_or("left unread".to_string(), |x| format!(
                    "{x} predicted, {} of them held",
                    c.held
                ))
            ));
        }
    }
    if r.spill_pages_written > 0 || r.blocks_pruned > 0 {
        out.push(format!(
            "  Past its room: {} pages written, {} read back; {} blocks pruned, {} pages written",
            r.spill_pages_written, r.spill_pages_read, r.blocks_pruned, r.pages_written
        ));
    }
}

/// Lines as a JSON array of strings, the one row FORMAT JSON gives.
fn as_json(lines: &[String]) -> String {
    let mut out = String::from("[");
    for (k, l) in lines.iter().enumerate() {
        if k > 0 {
            out.push_str(", ");
        }
        out.push('"');
        for ch in l.chars() {
            match ch {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                c => out.push(c),
            }
        }
        out.push('"');
    }
    out.push(']');
    out
}

/// What EXPLAIN runs a statement on: one snail, or several.
pub(super) trait Runs {
    fn ask_round(&mut self, sql: &str) -> Result<(arrow::array::RecordBatch, Round)>;
    fn run(&mut self, sql: &str) -> Result<super::Done>;
    fn priced(&mut self, sql: &str) -> Result<super::Price>;
}

impl Runs for Snail {
    fn ask_round(&mut self, sql: &str) -> Result<(arrow::array::RecordBatch, Round)> {
        self.ask(sql, None)
    }
    fn run(&mut self, sql: &str) -> Result<super::Done> {
        self.execute(sql)
    }
    fn priced(&mut self, sql: &str) -> Result<super::Price> {
        self.price(sql, None)
    }
}

/// EXPLAIN: the plan's lines, as rows of `QUERY PLAN`.
pub(super) fn explain(r: &mut dyn Runs, e: &Explain) -> Result<super::Done> {
    {
        let first = e
            .statement
            .split(|c: char| !c.is_ascii_alphabetic())
            .find(|w| !w.is_empty())
            .unwrap_or("")
            .to_ascii_uppercase();
        let question = matches!(first.as_str(), "SELECT" | "VALUES" | "WITH" | "TABLE");
        if !matches!(
            first.as_str(),
            "SELECT" | "VALUES" | "WITH" | "TABLE" | "INSERT" | "UPDATE" | "DELETE" | "EXECUTE"
        ) {
            return Err(Error::new(format!(
                "syntax error at or near \"{first}\""
            )));
        }
        let mut lines = Vec::new();
        let began = std::time::Instant::now();
        match (e.analyze, question) {
            (false, true) => {
                let p = r.priced(&e.statement).map_err(|why| {
                    Error::new(format!(
                        "EXPLAIN without ANALYZE prices a question on one table from its directory \
                         alone: {why}; EXPLAIN ANALYZE runs it and prints its round"
                    ))
                })?;
                lines.push(format!(
                    "Snail round along {}: {} to {} rows, about {}",
                    order_shown(&p.order), p.rows_low, p.rows_high, p.rows_estimate
                ));
                if e.costs {
                    lines.push(format!(
                        "  Pages: {} of the directory, {} of page maps, {} of columns predicted; {} \
                         blocks",
                        p.directory_pages, p.map_pages, p.column_pages, p.blocks
                    ));
                }
                if e.verbose {
                    for c in &p.consulted {
                        lines.push(format!(
                            "  Order {}: {} pages of its directory; {} to {} column pages by its \
                             directory, {}",
                            order_shown(&c.order),
                            c.directory_pages,
                            c.low,
                            c.high,
                            c.exact
                                .map_or("left unread".to_string(), |x| format!("{x} predicted"))
                        ));
                    }
                }
            }
            (false, false) => {
                return Err(Error::new(
                    "EXPLAIN of a write without ANALYZE is refused: a write's pages are known as it \
                     runs; EXPLAIN ANALYZE runs it and prints them",
                ))
            }
            (true, true) => {
                let (batch, round) = r.ask_round(&e.statement)?;
                let mut r = round;
                r.rows = batch.num_rows() as u64;
                round_lines(&r, e, &mut lines);
            }
            (true, false) => {
                let d = r.run(&e.statement)?;
                lines.push(format!("Snail write: {} {} rows", d.statement, d.rows));
                if e.costs {
                    lines.push(format!(
                        "  Pages: {} read, {} written ({} into pages freed before); {} blocks \
                         rewritten",
                        d.pages_read, d.pages_written, d.pages_reused, d.rewritten
                    ));
                }
            }
        }
        if e.analyze && e.summary != Some(false) {
            lines.push(format!(
                "Execution Time: {:.3} ms",
                began.elapsed().as_secs_f64() * 1000.0
            ));
        }
        let (column, rows): (&str, Vec<String>) = match e.json {
            true => ("QUERY PLAN", vec![as_json(&lines)]),
            false => ("QUERY PLAN", lines),
        };
        let n = rows.len() as u64;
        let batch = arrow::array::RecordBatch::try_new(
            std::sync::Arc::new(arrow::datatypes::Schema::new(vec![
                arrow::datatypes::Field::new(column, arrow::datatypes::DataType::Utf8, false),
            ])),
            vec![std::sync::Arc::new(arrow::array::StringArray::from(rows))],
        )?;
        Ok(super::Done {
            statement: "EXPLAIN",
            rows: n,
            answer: Some(batch),
            ..Default::default()
        })
    }
}
