//! The sequence functions a statement calls, as PostgreSQL's sequence.c answers them for a session:
//! `nextval`, `currval`, `setval` and `lastval`, each read from a tab the session lends the
//! statement while it runs, and `pg_get_serial_sequence`, from the catalog. A call that moves a
//! sequence is recorded on the tab in the order the statement makes it. Where the tab could not
//! answer one from what the session holds, the session replays them at the root's turn, as
//! PostgreSQL's never roll back, and runs the statement again over what they came to.

use super::pipeline::Value;
use crate::catalog::{Catalog, ColumnType, Sequence};
use crate::oids::{full, Object};
use crate::{Error, Result};
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};

/// What a session holds of its sequences beside the values they handed it and it has not used:
/// the value each last gave it, which `currval` reads, and the sequence it drew from last, whose
/// value `lastval` reads. Neither is rolled back. Each is held by the sequence's OID beside its
/// name, as PostgreSQL's session holds it by the relation: a sequence dropped and made again under
/// the name has handed this session nothing, and one a rollback brings back has.
#[derive(Clone, Debug, Default)]
pub(super) struct Drawn {
    current: HashMap<String, (u32, i64)>,
    last: Option<String>,
}

impl Drawn {
    /// A value a sequence handed the session: its `currval`, and the sequence `lastval` reads.
    pub(super) fn drew(&mut self, name: &str, oid: u32, v: i64) {
        self.current.insert(name.to_string(), (oid, v));
        self.last = Some(name.to_string());
    }

    /// The value the sequence of that name and OID last handed the session.
    fn current(&self, name: &str, oid: Option<u32>) -> Option<i64> {
        self.current
            .get(name)
            .filter(|(o, _)| Some(*o) == oid)
            .map(|(_, v)| *v)
    }
}

/// A sequence's OID, where the catalog holds it.
pub(super) fn sequence_oid(catalog: &Catalog, name: &str) -> Option<u32> {
    catalog.oids.of(&Object::Class(full(name)))
}

/// A call that moves a sequence, as the statement made it: a value drawn, or the sequence set to a
/// value, handed out or not.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Op {
    Draw(String),
    Set(String, i64, bool),
}

/// What a call came to: its value, or the words it was refused in.
pub(super) type Outcome = std::result::Result<i64, String>;

/// A call the script did not answer: what it was, what the run was answered, and whether only the
/// file can say what it came to, a value the session did not hold drawn or a sequence set.
#[derive(Clone, Debug)]
pub(super) struct Fresh {
    pub(super) op: Op,
    pub(super) outcome: Outcome,
    pub(super) owed: bool,
}

/// What a statement's calls read and record while it runs.
pub(super) struct Tab {
    drawn: Drawn,
    /// The values each sequence handed the session, those its draws take first.
    held: HashMap<String, VecDeque<i64>>,
    /// Where each sequence stands in the file as the run goes on: the value it handed out last, or
    /// hands out next where none was, and whether it has handed it out.
    stands: HashMap<String, (i64, bool)>,
    sequences: Vec<Sequence>,
    /// Each sequence's OID, by its name.
    oids: HashMap<String, u32>,
    /// The file's relations that are no sequence, by their full names, each with the words
    /// PostgreSQL names its kind by.
    others: HashMap<String, &'static str>,
    release: u8,
    /// Whether nothing may be written: a READ ONLY transaction, or a snail opened to read.
    read_only: bool,
    /// The calls a replay made, each with what it came to, which a run answers in order while it
    /// makes the same calls.
    script: Vec<(Op, Outcome)>,
    at: usize,
    diverged: bool,
    fresh: Vec<Fresh>,
}

/// What a statement's tab is made of.
pub(super) struct Lent<'a> {
    pub(super) drawn: Drawn,
    pub(super) held: &'a HashMap<String, VecDeque<i64>>,
    pub(super) stands: &'a HashMap<String, (i64, bool)>,
    pub(super) catalog: &'a Catalog,
    pub(super) read_only: bool,
    pub(super) script: Vec<(Op, Outcome)>,
}

impl Tab {
    pub(super) fn lent(l: Lent<'_>) -> Tab {
        let mut others: HashMap<String, &'static str> = HashMap::new();
        for t in &l.catalog.tables {
            others.insert(full(&t.name), "tables");
        }
        for v in &l.catalog.views {
            others.insert(full(&v.name), "views");
        }
        for t in &l.catalog.types {
            others.entry(full(&t.name)).or_insert("composite types");
        }
        let oids = l
            .catalog
            .sequences
            .iter()
            .filter_map(|s| Some((s.name.clone(), sequence_oid(l.catalog, &s.name)?)))
            .collect();
        Tab {
            drawn: l.drawn,
            held: l.held.clone(),
            stands: l.stands.clone(),
            sequences: l.catalog.sequences.clone(),
            oids,
            others,
            release: l.catalog.release,
            read_only: l.read_only,
            script: l.script,
            at: 0,
            diverged: false,
            fresh: Vec::new(),
        }
    }

    /// What the run left the session: its `currval`s and the sequence it drew from last.
    pub(super) fn drawn(&self) -> &Drawn {
        &self.drawn
    }

    /// The values each sequence handed the session, as the run's draws left them.
    pub(super) fn held(self) -> HashMap<String, VecDeque<i64>> {
        self.held
    }

    /// The calls the script did not answer, in order.
    pub(super) fn fresh(&self) -> &[Fresh] {
        &self.fresh
    }

    /// How many of the script's calls the run made before it made another.
    pub(super) fn matched(&self) -> usize {
        self.at
    }

    /// Whether only a replay can say what the run's calls came to.
    pub(super) fn owes(&self) -> bool {
        self.fresh.iter().any(|f| f.owed)
    }

    fn sequence(&self, name: &str) -> &Sequence {
        self.sequences
            .iter()
            .find(|s| s.name == name)
            .expect("a sequence the tab named")
    }

    /// The call's outcome, from the script where it is the script's next, else as the tab answers it.
    fn made(&mut self, op: Op, fresh: impl FnOnce(&mut Tab) -> (Outcome, bool)) -> Outcome {
        if !self.diverged {
            if let Some((o, out)) = self.script.get(self.at) {
                if *o == op {
                    self.at += 1;
                    let out = out.clone();
                    if let Ok(v) = out {
                        self.took(&op, v);
                    }
                    return out;
                }
            }
            self.diverged = !self.script.is_empty();
        }
        let (outcome, owed) = fresh(self);
        if let Ok(v) = outcome {
            self.took(&op, v);
        }
        self.fresh.push(Fresh {
            op,
            outcome: outcome.clone(),
            owed,
        });
        outcome
    }

    /// What a call that came to `v` leaves the session.
    fn took(&mut self, op: &Op, v: i64) {
        let oid = |s: &String| self.oids.get(s).copied().unwrap_or(0);
        match op {
            Op::Draw(s) => {
                let o = oid(s);
                self.drawn.drew(s, o, v)
            }
            Op::Set(s, _, true) => {
                let o = oid(s);
                self.drawn.current.insert(s.clone(), (o, v));
            }
            Op::Set(..) => {}
        }
    }

    /// nextval: the value the session holds first; else the one the sequence's own arithmetic gives
    /// next from where the file stands, which only a replay can confirm.
    fn draw(&mut self, name: &str) -> Outcome {
        self.made(Op::Draw(name.to_string()), |tab| {
            if let Some(v) = tab.held.get_mut(name).and_then(VecDeque::pop_front) {
                return (Ok(v), false);
            }
            let s = tab.sequence(name).clone();
            let stands = tab
                .stands
                .get(name)
                .copied()
                .unwrap_or((s.last_value, s.is_called));
            match next(&s, stands) {
                Ok(v) => {
                    tab.stands.insert(name.to_string(), (v, true));
                    (Ok(v), true)
                }
                Err(e) => (Err(e), true),
            }
        })
    }

    /// setval: the sequence set to `v`, the values the session held of it thrown away.
    fn set(&mut self, name: &str, v: i64, called: bool) -> Outcome {
        self.made(Op::Set(name.to_string(), v, called), |tab| {
            tab.held.remove(name);
            tab.stands.insert(name.to_string(), (v, called));
            (Ok(v), true)
        })
    }

    /// The sequence a regclass argument names, or none for a NULL; refused in PostgreSQL's words
    /// where it names no relation, or one that is no sequence.
    fn named(&self, v: &Value) -> Result<Option<String>> {
        let oid = match v {
            Value::Null => return Ok(None),
            Value::Text(s) | Value::Str(_, s) => self.by_text(s)?,
            v @ Value::Scalar(ColumnType::Oid | ColumnType::Reg(..), _) => {
                crate::deparse::oid_of(v).unwrap_or(0)
            }
            Value::Int(n) => u32::try_from(*n).unwrap_or(0),
            other => {
                return Err(Error::new(format!(
                    "a {} names no sequence",
                    super::pipeline::kind_name(other)
                )))
            }
        };
        self.by_oid(oid).map(Some)
    }

    /// A relation's name as regclass's input reads it: folded unless quoted, a name alone found on
    /// the search path.
    fn by_text(&self, s: &str) -> Result<u32> {
        let parts = crate::regs::split_name(s).ok_or_else(|| Error::new("invalid name syntax"))?;
        match parts.len() {
            1 | 2 => {}
            3 => {
                return Err(Error::new(format!(
                    "cross-database references are not implemented: \"{}\"",
                    parts.join(".")
                )))
            }
            _ => {
                return Err(Error::new(format!(
                    "improper relation name (too many dotted names): {}",
                    parts.join(".")
                )))
            }
        }
        let joined = parts.join(".");
        crate::regs::class_of_text(s, self.release).map_err(|_| {
            match super::sysviews::view_named(&joined) {
                Some(_) => Error::new(format!(
                    "cannot open relation \"{}\": This operation is not supported for views.",
                    parts.last().expect("a name")
                )),
                None => Error::new(format!("relation \"{joined}\" does not exist")),
            }
        })
    }

    /// The sequence an OID names, or PostgreSQL's words for what it names instead.
    fn by_oid(&self, oid: u32) -> Result<String> {
        let refused = |name: &str, kind: &str| {
            Error::new(format!(
                "cannot open relation \"{}\": This operation is not supported for {kind}.",
                name.rsplit('.').next().unwrap_or(name)
            ))
        };
        if let Some(Object::Class(n)) = crate::oids::held_object(oid)? {
            if let Some(s) = self.sequences.iter().find(|s| full(&s.name) == n) {
                return Ok(s.name.clone());
            }
            return Err(refused(&n, self.others.get(&n).copied().unwrap_or("indexes")));
        }
        let text = crate::regs::reg_text(crate::regs::RegKind::Class, oid, self.release);
        if text.bytes().all(|b| b.is_ascii_digit()) {
            return Err(Error::new(format!("could not open relation with OID {oid}")));
        }
        let kind = match super::sysviews::view_named(&text) {
            Some(_) => "views",
            None => "tables",
        };
        Err(refused(&text, kind))
    }
}

/// The value a sequence hands out after where it stands: the one it stands on where it has not
/// handed that out, else the next by its increment, from its other end where it cycles; refused in
/// PostgreSQL's words where it runs out.
pub(super) fn next(s: &Sequence, (last, called): (i64, bool)) -> Outcome {
    if !called {
        return Ok(last);
    }
    if let Some(v) = last.checked_add(s.increment).filter(|v| *v >= s.min && *v <= s.max) {
        return Ok(v);
    }
    if s.cycle {
        return Ok(if s.increment > 0 { s.min } else { s.max });
    }
    let (end, bound) = if s.increment > 0 {
        ("maximum", s.max)
    } else {
        ("minimum", s.min)
    };
    Err(format!(
        "nextval: reached {end} value of sequence \"{}\" ({bound})",
        s.name.rsplit('.').next().unwrap_or(&s.name)
    ))
}

/// setval's refusal of a value past a sequence's bounds, in PostgreSQL's words.
pub(super) fn out_of_bounds(s: &Sequence, v: i64) -> Option<String> {
    (v < s.min || v > s.max).then(|| {
        format!(
            "setval: value {v} is out of bounds for sequence \"{}\" ({}..{})",
            s.name.rsplit('.').next().unwrap_or(&s.name),
            s.min,
            s.max
        )
    })
}

thread_local! {
    /// The tab of the statement under way on this thread, where it calls the sequence functions.
    static TAB: RefCell<Option<Tab>> = const { RefCell::new(None) };
    /// Whether a question is read only for what it returns, its rows thrown away: a subquery's
    /// columns learnt before any row asks it.
    static PROBING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// A question read only for the columns it returns, until dropped: its calls move no sequence and
/// answer a bigint of no account.
pub(super) struct Probing(bool);

pub(super) fn probing() -> Probing {
    Probing(PROBING.with(|p| p.replace(true)))
}

impl Drop for Probing {
    fn drop(&mut self) {
        PROBING.with(|p| p.set(self.0));
    }
}

/// A statement's tab lent until it ends or is dropped.
pub(super) struct Tabbing(Option<Option<Tab>>);

pub(super) fn tabbing(tab: Tab) -> Tabbing {
    Tabbing(Some(TAB.with(|t| t.replace(Some(tab)))))
}

impl Tabbing {
    /// The tab as the statement left it.
    pub(super) fn ended(mut self) -> Tab {
        let before = self.0.take().expect("a tab lent");
        TAB.with(|t| std::mem::replace(&mut *t.borrow_mut(), before))
            .expect("the statement's tab")
    }
}

impl Drop for Tabbing {
    fn drop(&mut self) {
        if let Some(before) = self.0.take() {
            TAB.with(|t| *t.borrow_mut() = before);
        }
    }
}

/// Whether a statement's text names one of the functions a tab answers.
pub(super) fn names(sql: &str) -> bool {
    let lower = sql.to_ascii_lowercase();
    ["nextval", "setval", "currval", "lastval"]
        .iter()
        .any(|n| lower.contains(n))
}

/// Draws `n` values of a sequence for a column's DEFAULT on the statement's tab, where it has one.
pub(super) fn defaults(name: &str, n: usize) -> Option<Result<Vec<i64>>> {
    TAB.with(|t| {
        let mut t = t.borrow_mut();
        let tab = t.as_mut()?;
        Some(
            (0..n)
                .map(|_| tab.draw(name).map_err(Error::new))
                .collect(),
        )
    })
}

/// What `call` answers in: a bigint, or text for `pg_get_serial_sequence`.
pub(super) fn typed(name: &str) -> Option<ColumnType> {
    match name {
        "nextval" | "currval" | "setval" | "lastval" => Some(ColumnType::Int8),
        "pg_get_serial_sequence" => Some(ColumnType::Text(None)),
        _ => None,
    }
}

/// A call of one of the sequence functions, as PostgreSQL answers it; none for any other.
pub(super) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    match (name, args.len()) {
        ("nextval" | "currval", 1) | ("lastval", 0) | ("setval", 2 | 3) => {}
        ("pg_get_serial_sequence", 2) => return serial(args).map(Some),
        _ => return Ok(None),
    }
    // Each is strict: a NULL argument answers NULL, and moves nothing.
    if args.iter().any(Value::is_null) {
        return Ok(Some(Value::Null));
    }
    if PROBING.with(std::cell::Cell::get) {
        return Ok(Some(Value::Int(0)));
    }
    let set_to = match args.get(1).filter(|_| name == "setval") {
        Some(Value::Int(v)) => Some(*v),
        Some(v @ (Value::Text(_) | Value::Str(..))) => {
            Some(crate::composed::int_in(&v.text().unwrap_or_default(), 64)?)
        }
        Some(other) => {
            return Err(Error::new(format!(
                "function setval(unknown, {}) does not exist",
                super::pipeline::kind_name(other)
            )))
        }
        None => None,
    };
    let called = match args.get(2) {
        None | Some(Value::Bool(true)) => true,
        Some(Value::Bool(false)) => false,
        Some(v) => {
            let t = v.text().unwrap_or_default();
            match crate::read::truth(&t) {
                Some(b) => b,
                None => {
                    return Err(Error::new(format!(
                        "invalid input syntax for type boolean: \"{t}\""
                    )))
                }
            }
        }
    };
    TAB.with(|t| {
        let mut t = t.borrow_mut();
        let Some(tab) = t.as_mut() else {
            return Err(Error::new(format!(
                "{name}() reads the session's sequences, which only a statement run whole on one \
                 snail lends it"
            )));
        };
        let v = match name {
            "lastval" => {
                let last = tab.drawn.last.clone();
                match last.and_then(|s| tab.drawn.current(&s, tab.oids.get(&s).copied())) {
                    Some(v) => v,
                    None => return Err(Error::new("lastval is not yet defined in this session")),
                }
            }
            _ => {
                let s = tab.named(&args[0])?.expect("an argument not NULL");
                match name {
                    "currval" => match tab.drawn.current(&s, tab.oids.get(&s).copied()) {
                        Some(v) => v,
                        None => {
                            return Err(Error::new(format!(
                                "currval of sequence \"{}\" is not yet defined in this session",
                                s.rsplit('.').next().unwrap_or(&s)
                            )))
                        }
                    },
                    _ if tab.read_only => {
                        return Err(Error::new(format!(
                            "cannot execute {name}() in a read-only transaction"
                        )))
                    }
                    "nextval" => tab.draw(&s).map_err(Error::new)?,
                    _ => {
                        let v = set_to.expect("setval's value");
                        if let Some(e) = out_of_bounds(tab.sequence(&s), v) {
                            return Err(Error::new(e));
                        }
                        tab.set(&s, v, called).map_err(Error::new)?
                    }
                }
            }
        };
        Ok(Some(Value::Int(v)))
    })
}

/// pg_get_serial_sequence(table, column): the sequence the column owns, a serial's, an identity's
/// or one OWNED BY it, schema-qualified and quoted as quote_ident quotes; NULL where none. The
/// table's name is read as regclass reads it, the column's taken as written.
fn serial(args: &[Value]) -> Result<Value> {
    let (Some(table), Some(column)) = (args[0].text(), args[1].text()) else {
        return Ok(Value::Null);
    };
    let catalog = crate::deparse::held()?;
    let parts = crate::regs::split_name(&table).ok_or_else(|| Error::new("invalid name syntax"))?;
    let missing = || Error::new(format!("relation \"{}\" does not exist", parts.join(".")));
    let oid = match parts.len() {
        1 | 2 => crate::regs::class_of_text(&table, catalog.release).map_err(|_| missing())?,
        3 => {
            return Err(Error::new(format!(
                "cross-database references are not implemented: \"{}\"",
                parts.join(".")
            )))
        }
        _ => {
            return Err(Error::new(format!(
                "improper relation name (too many dotted names): {}",
                parts.join(".")
            )))
        }
    };
    let Some(Object::Class(named)) = crate::oids::held_object(oid)? else {
        return Ok(Value::Null);
    };
    let bare = named.rsplit('.').next().unwrap_or(&named).to_string();
    let columns: Option<Vec<&str>> = match catalog.tables.iter().find(|t| full(&t.name) == named) {
        Some(t) => Some(t.columns.iter().map(|c| c.name.as_str()).collect()),
        None => catalog
            .views
            .iter()
            .find(|v| full(&v.name) == named)
            .map(|v| v.columns.iter().map(String::as_str).collect()),
    };
    if !columns.is_some_and(|cs| cs.contains(&column.as_str())) {
        return Err(Error::new(format!(
            "column \"{column}\" of relation \"{bare}\" does not exist"
        )));
    }
    let owned = catalog.sequences.iter().find(|s| {
        s.owned_by
            .as_ref()
            .is_some_and(|(t, c)| full(t) == named && *c == column)
    });
    Ok(match owned {
        Some(s) => {
            let named = full(&s.name);
            let (schema, name) = crate::oids::parts(&named);
            Value::Text(format!(
                "{}.{}",
                super::calls::quote_ident(schema),
                super::calls::quote_ident(name)
            ))
        }
        None => Value::Null,
    })
}
