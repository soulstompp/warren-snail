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
    /// SET of a setting, LOCAL or of the session, to its values, or to its default where none.
    Set {
        name: String,
        values: Option<Vec<String>>,
        local: bool,
    },
    /// SHOW of a setting, or of them all.
    Show {
        name: Option<String>,
    },
    /// RESET of a setting, or of them all.
    Reset {
        name: Option<String>,
    },
    /// PREPARE: its name, its parameters' types as it names them, and its statement.
    Prepare {
        name: String,
        types: Vec<Option<String>>,
        statement: String,
    },
    /// EXECUTE: the prepared statement's name and each parameter's expression.
    Execute { name: String, args: Vec<String> },
    /// DEALLOCATE of a prepared statement, or of them all.
    Deallocate { name: Option<String> },
    /// DECLARE of a cursor.
    Declare(super::cursor::Declaration),
    /// FETCH, or MOVE where `fetch` is false: where it takes the cursor, and the cursor's name.
    Fetch {
        fetch: bool,
        direction: super::cursor::Direction,
        name: String,
    },
    /// CLOSE of a cursor, or of them all.
    Close { name: Option<String> },
    /// COPY.
    Copy(Box<super::copy::Copy>),
    /// EXPLAIN.
    Explain(Box<super::explain::Explain>),
}

/// The words of a statement but its white space and its last semicolon.
fn words(tokens: &[sqlparser::tokenizer::Token]) -> Vec<&sqlparser::tokenizer::Token> {
    use sqlparser::tokenizer::Token;
    let mut w: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_) | Token::EOF))
        .collect();
    if matches!(w.last(), Some(Token::SemiColon)) {
        w.pop();
    }
    w
}

/// A word as PostgreSQL's grammar reads a keyword: upper case, none where it is quoted or no word.
fn keyword(t: Option<&&sqlparser::tokenizer::Token>) -> String {
    match t {
        Some(sqlparser::tokenizer::Token::Word(w)) if w.quote_style.is_none() => {
            w.value.to_uppercase()
        }
        _ => String::new(),
    }
}

fn syntax_at(t: Option<&&sqlparser::tokenizer::Token>) -> Error {
    Error::new(match t {
        Some(t) => format!("syntax error at or near \"{t}\""),
        None => "syntax error at end of input".to_string(),
    })
}

/// A setting's name as SET, SHOW and RESET read it: a word, or words with dots between, each lower
/// case unless quoted; and the place after it.
fn setting_name(w: &[&sqlparser::tokenizer::Token], mut i: usize) -> Result<(String, usize)> {
    use sqlparser::tokenizer::Token;
    let mut parts = Vec::new();
    loop {
        match w.get(i) {
            Some(Token::Word(x)) if x.quote_style.is_some() => parts.push(x.value.clone()),
            // A reserved word, or one that names a type or a function alone, is no part of a name.
            Some(Token::Word(x)) if !col_id(&x.value) => return Err(syntax_at(w.get(i))),
            Some(Token::Word(x)) => parts.push(x.value.to_lowercase()),
            other => return Err(syntax_at(other)),
        }
        i += 1;
        if matches!(w.get(i), Some(Token::Period)) {
            i += 1;
            continue;
        }
        return Ok((parts.join("."), i));
    }
}

/// SET's values, as PostgreSQL's `var_list` reads them: each a word lower case unless quoted, a
/// string, or a number with its sign.
fn set_values(w: &[&sqlparser::tokenizer::Token], mut i: usize) -> Result<Vec<String>> {
    use sqlparser::tokenizer::Token;
    let mut out = Vec::new();
    loop {
        let mut sign = "";
        if matches!(w.get(i), Some(Token::Minus | Token::Plus)) {
            sign = if matches!(w.get(i), Some(Token::Minus)) {
                "-"
            } else {
                ""
            };
            i += 1;
        }
        out.push(match w.get(i) {
            Some(Token::Word(x)) if sign.is_empty() => match x.quote_style {
                Some(_) => x.value.clone(),
                None => x.value.to_lowercase(),
            },
            Some(Token::SingleQuotedString(s)) | Some(Token::EscapedStringLiteral(s))
                if sign.is_empty() =>
            {
                s.clone()
            }
            Some(Token::Number(n, _)) => format!("{sign}{n}"),
            other => return Err(syntax_at(other)),
        });
        i += 1;
        match w.get(i) {
            None => return Ok(out),
            Some(Token::Comma) => i += 1,
            other => return Err(syntax_at(other)),
        }
    }
}

/// SET, SHOW and RESET read off their words; none where the words are no such statement, or one
/// the snail reads elsewhere: SET TRANSACTION, SET SESSION CHARACTERISTICS and SET CONSTRAINTS.
fn parse_setting(tokens: &[sqlparser::tokenizer::Token]) -> Option<Result<Said>> {
    let w = words(tokens);
    let first = keyword(w.first());
    let refused =
        |what: &str, why: &str| Some(Err(Error::new(format!("{what} is refused: {why}"))));
    let roles = "roles and privileges are the server's own, and a snail has none";
    match first.as_str() {
        "SET" => {
            let mut i = 1;
            let mut local = false;
            match keyword(w.get(i)).as_str() {
                "LOCAL" => {
                    local = true;
                    i += 1;
                }
                "SESSION"
                    if keyword(w.get(i + 1)) != "AUTHORIZATION"
                        && keyword(w.get(i + 1)) != "CHARACTERISTICS" =>
                {
                    i += 1
                }
                _ => {}
            }
            let named = |name: &str, values: Option<Vec<String>>| {
                Some(Ok(Said::Set {
                    name: name.to_string(),
                    values,
                    local,
                }))
            };
            match (keyword(w.get(i)).as_str(), keyword(w.get(i + 1)).as_str()) {
                ("TRANSACTION", _) | ("CONSTRAINTS", _) | ("SESSION", "CHARACTERISTICS") => None,
                ("ROLE", _) => refused("SET ROLE", roles),
                ("SESSION", "AUTHORIZATION") => refused("SET SESSION AUTHORIZATION", roles),
                ("TIME", "ZONE") => match keyword(w.get(i + 2)).as_str() {
                    "DEFAULT" | "LOCAL" if w.len() == i + 3 => named("timezone", None),
                    _ => match set_values(&w, i + 2) {
                        Ok(v) => named("timezone", Some(v)),
                        Err(e) => Some(Err(e)),
                    },
                },
                ("NAMES", _) => match keyword(w.get(i + 1)).as_str() {
                    "DEFAULT" if w.len() == i + 2 => named("client_encoding", None),
                    _ => match set_values(&w, i + 1) {
                        Ok(v) => named("client_encoding", Some(v)),
                        Err(e) => Some(Err(e)),
                    },
                },
                ("SCHEMA", _) => match set_values(&w, i + 1) {
                    Ok(v) => named("search_path", Some(v)),
                    Err(e) => Some(Err(e)),
                },
                ("XML", "OPTION") => match keyword(w.get(i + 2)).as_str() {
                    v @ ("DOCUMENT" | "CONTENT") if w.len() == i + 3 => {
                        named("xmloption", Some(vec![v.to_lowercase()]))
                    }
                    _ => Some(Err(syntax_at(w.get(i + 2)))),
                },
                ("SEED", _) => refused(
                    "SET SEED",
                    "the snail's random() draws from no seed a session sets",
                ),
                _ => {
                    let (name, j) = match setting_name(&w, i) {
                        Ok(x) => x,
                        Err(e) => return Some(Err(e)),
                    };
                    use sqlparser::tokenizer::Token;
                    let j = match w.get(j) {
                        Some(Token::Eq) => j + 1,
                        Some(Token::Word(x))
                            if x.quote_style.is_none() && x.value.eq_ignore_ascii_case("to") =>
                        {
                            j + 1
                        }
                        Some(Token::Word(x))
                            if x.quote_style.is_none()
                                && x.value.eq_ignore_ascii_case("from")
                                && keyword(w.get(j + 1)) == "CURRENT"
                                && w.len() == j + 2 =>
                        {
                            return refused(
                                &format!("SET {name} FROM CURRENT"),
                                "it names no value of its own; SET it to one",
                            )
                        }
                        other => return Some(Err(syntax_at(other))),
                    };
                    if keyword(w.get(j)) == "DEFAULT" && w.len() == j + 1 {
                        return named(&name, None);
                    }
                    match set_values(&w, j) {
                        Ok(v) => named(&name, Some(v)),
                        Err(e) => Some(Err(e)),
                    }
                }
            }
        }
        "SHOW" | "RESET" => {
            let name = match (keyword(w.get(1)).as_str(), keyword(w.get(2)).as_str()) {
                ("ALL", "") => None,
                ("TIME", "ZONE") if w.len() == 3 => Some("timezone".to_string()),
                ("TRANSACTION", "ISOLATION") if keyword(w.get(3)) == "LEVEL" && w.len() == 4 => {
                    Some("transaction_isolation".to_string())
                }
                ("SESSION", "AUTHORIZATION") => {
                    return refused(&format!("{first} SESSION AUTHORIZATION"), roles)
                }
                ("ROLE", "") => return refused(&format!("{first} ROLE"), roles),
                _ => match setting_name(&w, 1) {
                    Ok((n, j)) if j == w.len() => Some(n),
                    Ok((_, j)) => return Some(Err(syntax_at(w.get(j)))),
                    Err(e) => return Some(Err(e)),
                },
            };
            Some(Ok(match first.as_str() {
                "SHOW" => Said::Show { name },
                _ => Said::Reset { name },
            }))
        }
        _ => None,
    }
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
            Said::Set { .. } => "SET",
            Said::Show { .. } => "SHOW",
            Said::Reset { .. } => "RESET",
            Said::Prepare { .. } => "PREPARE",
            Said::Execute { .. } => "EXECUTE",
            Said::Deallocate { .. } => "DEALLOCATE",
            Said::Declare(_) => "DECLARE CURSOR",
            Said::Fetch { fetch: true, .. } => "FETCH",
            Said::Fetch { .. } => "MOVE",
            Said::Close { .. } => "CLOSE CURSOR",
            Said::Copy(_) => "COPY",
            Said::Explain(_) => "EXPLAIN",
        }
    }
}

/// A statement of the session's own read off its words, where sqlparser does not read it as
/// PostgreSQL does: none where it is no such statement.
pub(super) fn said(
    catalog: &Catalog,
    sql: &str,
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
    if let Some(read) = parse_setting(tokens) {
        return read.map(Some);
    }
    if let Some(read) = super::prepared::parse_prepare(sql) {
        let (name, types, statement) = read?;
        return Ok(Some(Said::Prepare {
            name,
            types,
            statement,
        }));
    }
    if let Some(read) = super::cursor::parse_declare(sql) {
        return Ok(Some(Said::Declare(read?)));
    }
    if let Some(read) = super::cursor::parse_fetch(tokens) {
        let (fetch, direction, name) = read?;
        return Ok(Some(Said::Fetch {
            fetch,
            direction,
            name,
        }));
    }
    if let Some(read) = super::explain::parse_explain(sql) {
        return Ok(Some(Said::Explain(Box::new(read?))));
    }
    if let Some(read) = super::copy::parse_copy(catalog, sql) {
        return Ok(Some(Said::Copy(Box::new(read?))));
    }
    if let Some(read) = parse_execute(sql) {
        return read.map(Some);
    }
    Ok(None)
}

/// A writer's session.
#[derive(Default)]
pub(super) struct Session {
    pub(super) settings: Settings,
    pub(super) prepared: super::prepared::Kept,
    pub(super) cursors: Vec<super::cursor::Cursor>,
    pub(super) table_locks: BTreeSet<(usize, Mode)>,
    /// The cursors declared so far, which numbers each.
    pub(super) cursor_count: u64,
    /// The rows a caller hands COPY … FROM STDIN.
    pub(super) copy_input: Option<Vec<u8>>,
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
        // A rollback closes the cursors the transaction declared, held or not, and leaves those held
        // from before it.
        self.cursors
            .retain(|c| c.hold && (committed || !c.fresh));
        for c in &mut self.cursors {
            c.fresh = false;
        }
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
        let mut done = super::Done::default();
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
            Said::Set {
                name,
                values,
                local,
            } => {
                if local && self.begun.is_none() {
                    done.notices
                        .push("WARNING:  SET LOCAL can only be used in transaction blocks".into());
                }
                self.set_setting(&name, values, local)?
            }
            Said::Show { name } => {
                let (column, value) = self.shown_setting(name.as_deref())?;
                let batch = arrow::array::RecordBatch::try_new(
                    std::sync::Arc::new(arrow::datatypes::Schema::new(vec![
                        arrow::datatypes::Field::new(
                            column,
                            arrow::datatypes::DataType::Utf8,
                            false,
                        ),
                    ])),
                    vec![std::sync::Arc::new(arrow::array::StringArray::from(vec![
                        value,
                    ]))],
                )?;
                return Ok(super::Done {
                    rows: 1,
                    answer: Some(batch),
                    ..done
                });
            }
            Said::Reset { name: Some(name) } => self.set_setting(&name, None, false)?,
            Said::Prepare {
                name,
                types,
                statement,
            } => super::prepared::prepare(
                &mut self.session.prepared,
                &self.catalog,
                &name,
                types,
                &statement,
            )?,
            Said::Execute { name, args } => return self.execute_prepared(&name, &args),
            Said::Deallocate { name } => {
                super::prepared::deallocate(&mut self.session.prepared, name.as_deref())?
            }
            Said::Declare(d) => self.declare_cursor(d)?,
            Said::Fetch {
                fetch,
                direction,
                name,
            } => return self.fetch_cursor(fetch, &direction, &name),
            Said::Close { name } => self.close_cursor(name.as_deref())?,
            Said::Explain(e) => return super::explain::explain(self, &e),
            Said::Copy(c) => {
                return match c.from_stdin() || !c.to() {
                    true => {
                        let given = self.session.copy_input.take();
                        self.copy_from(&c, given.as_deref())
                    }
                    false => self.copy_to(&c),
                }
            }
            Said::Reset { name: None } => {
                let begun = self.begun.is_some();
                self.session.settings.reset_all(begun);
                self.levels.reset_session();
            }
        }
        Ok(done)
    }

    /// What set_config and current_setting read the session's settings and modes by.
    pub(super) fn settings_view(&self) -> super::settings::View {
        super::settings::View {
            release: self.catalog.release,
            monetary: self.catalog.monetary.name(),
            lc_monetary: self.catalog.monetary,
            text_search: self.catalog.text_search,
            values: self.session.settings.all().clone(),
            modes: [
                "transaction_isolation",
                "transaction_read_only",
                "transaction_deferrable",
                "default_transaction_isolation",
                "default_transaction_read_only",
                "default_transaction_deferrable",
            ]
            .into_iter()
            .map(|n| {
                (
                    n,
                    self.levels
                        .shown(n, self.begun.is_some())
                        .expect("a mode's setting"),
                )
            })
            .collect(),
            changed: Vec::new(),
        }
    }

    /// SET of a setting to `values`, or to its default where none, as its kind takes it.
    fn set_setting(&mut self, name: &str, values: Option<Vec<String>>, local: bool) -> Result<()> {
        let value = match values.as_deref() {
            None => None,
            Some([v]) => Some(v.clone()),
            Some(_) if name == "timezone" || name == "datestyle" => {
                Some(values.unwrap().join(", "))
            }
            Some(_) => return Err(Error::new(format!("SET {name} takes only one argument"))),
        };
        let taken = super::settings::taken(
            self.catalog.release,
            self.catalog.monetary,
            self.catalog.text_search,
            name,
            value.as_deref(),
        )?;
        self.take_setting(name, taken, local)
    }

    /// A setting taken: stored for the session or the transaction, or a transaction's mode set.
    pub(super) fn take_setting(
        &mut self,
        name: &str,
        taken: super::settings::Taken,
        local: bool,
    ) -> Result<()> {
        let begun = self.begun.is_some();
        match taken {
            super::settings::Taken::Nothing => {}
            // SET LOCAL outside a transaction lasts no longer than the statement.
            super::settings::Taken::Stored(shown) => {
                if !local || begun {
                    self.session.settings.set(name, shown, local, begun);
                }
            }
            super::settings::Taken::Mode(text) => {
                self.execute(&text)?;
            }
        }
        Ok(())
    }

    /// SHOW of a setting: its name as PostgreSQL's SHOW names its column, and its value.
    fn shown_setting(&self, name: Option<&str>) -> Result<(String, String)> {
        let Some(name) = name else {
            return Err(Error::new(
                "SHOW ALL is refused: it lists the server's settings, and a snail has no server; \
                 SHOW names one setting",
            ));
        };
        super::settings::shown(&self.settings_view(), name)
    }
}

/// EXECUTE name [(expression, …)], DEALLOCATE [PREPARE] {name | ALL} and CLOSE {name | ALL},
/// read off their words: none where the words are no such statement.
fn parse_execute(sql: &str) -> Option<Result<Said>> {
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
    let mut end = words.len();
    if matches!(words.last().map(|w| &w.0), Some(Token::SemiColon)) {
        end -= 1;
    }
    let syntax = |i: usize| {
        Some(Err(Error::new(match words.get(i) {
            Some((t, _, _)) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".into(),
        })))
    };
    let name_at = |i: usize| match words.get(i).map(|w| &w.0) {
        Some(Token::Word(w)) => Some(match w.quote_style {
            Some(_) => w.value.clone(),
            None => w.value.to_lowercase(),
        }),
        _ => None,
    };
    match kw(0).as_str() {
        "EXECUTE" => {
            let Some(name) = name_at(1) else {
                return syntax(1);
            };
            let mut args = Vec::new();
            if 2 < end {
                if !matches!(words[2].0, Token::LParen) || !matches!(words[end - 1].0, Token::RParen)
                {
                    return syntax(2);
                }
                let mut depth = 0;
                let mut from = words[2].2;
                for w in &words[3..end] {
                    match w.0 {
                        Token::LParen | Token::LBracket => depth += 1,
                        Token::RParen | Token::RBracket if depth > 0 => depth -= 1,
                        Token::Comma | Token::RParen if depth == 0 => {
                            let a = sql[from..w.1].trim();
                            if a.is_empty() {
                                return syntax(2);
                            }
                            args.push(a.to_string());
                            from = w.2;
                        }
                        _ => {}
                    }
                }
            }
            Some(Ok(Said::Execute { name, args }))
        }
        "DEALLOCATE" => {
            let i = if kw(1) == "PREPARE" { 2 } else { 1 };
            if i + 1 != end {
                return syntax(i + 1);
            }
            match kw(i).as_str() {
                "ALL" => Some(Ok(Said::Deallocate { name: None })),
                _ => match name_at(i) {
                    Some(n) => Some(Ok(Said::Deallocate { name: Some(n) })),
                    None => syntax(i),
                },
            }
        }
        "CLOSE" => {
            if end != 2 {
                return syntax(2);
            }
            match kw(1).as_str() {
                "ALL" => Some(Ok(Said::Close { name: None })),
                _ => match name_at(1) {
                    Some(n) => Some(Ok(Said::Close { name: Some(n) })),
                    None => syntax(1),
                },
            }
        }
        _ => None,
    }
}

/// Whether a word, unquoted, is a name PostgreSQL's grammar takes as a column's (`ColId`): any
/// word but its reserved keywords and those that name a type or a function alone.
fn col_id(word: &str) -> bool {
    let lower = word.to_lowercase();
    !include_str!("../kwlist.txt").lines().any(|l| {
        let mut f = l.split('\t');
        f.next() == Some(lower.as_str()) && matches!(f.next(), Some("R" | "T"))
    })
}

impl super::Snail {
    /// The statement_timeout and lock_timeout the session's next statement runs under.
    pub(super) fn timing(&self) -> super::settings::Timing {
        let all = self.session.settings.all();
        super::settings::timing(
            super::settings::millis(all, "statement_timeout"),
            super::settings::millis(all, "lock_timeout"),
        )
    }
}
