//! A statement that writes, in PostgreSQL's SQL, as the snail reads it: rows inserted from values or
//! from a question, rows a question finds updated or deleted, and the statements that begin and end
//! a transaction.

use super::question::{instant, number, text_literal, truth_literal};
use crate::catalog::{ident, object_name, Catalog, ColumnType, Table};
use crate::{Error, Result};
use sqlparser::ast::{
    AssignmentTarget, Expr, FromTable, Insert, ObjectName, ObjectNamePart, Query, SelectItem,
    SetExpr, Statement as Sql, TableFactor, TableObject, TableWithJoins, UnaryOperator, Value,
};
use sqlparser::dialect::PostgreSqlDialect;

/// One value of a row.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Datum {
    Null,
    /// An integer, an instant's microseconds, or a numeric's smallest units.
    Int(i64),
    Text(String),
    Bytes(Vec<u8>),
    /// A value keyed in 128 bits, as its key: a uuid's.
    Wide(i128),
    /// A real, as its own bits.
    Float4(u32),
    /// A double precision, as its own bits.
    Float8(u64),
    /// A value of a composed type, held whole.
    Compound(std::sync::Arc<crate::composed::Compound>),
    /// An interval, as its own fields.
    Interval(crate::times::Interval),
}

impl Datum {
    /// A number's value, or a wide value's or a float's key, in 128 bits.
    pub fn wide(&self) -> Option<i128> {
        match self {
            Datum::Int(v) => Some(*v as i128),
            Datum::Float4(b) => Some(crate::scalars::float4_key(*b) as i128),
            Datum::Float8(b) => Some(crate::scalars::float8_key(*b) as i128),
            Datum::Wide(k) => Some(*k),
            Datum::Interval(v) => Some(v.total()),
            _ => None,
        }
    }
}

/// A row of a table, its values in the table's column order.
pub type Row = Vec<Datum>;

/// Where an inserted row's value comes from: a literal, a column of the question's answer, or
/// the column's DEFAULT, which an identity's sequence or its DDL hands out, or else NULL; and
/// where an UPDATE sets one to an expression, the expression, read on the row's old values.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Fill {
    Literal(Datum),
    Answer(usize),
    Default,
    Expression(String),
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Source {
    Values(Vec<Row>),
    /// A question, and for each of the table's columns where its value comes from.
    Select {
        question: String,
        fill: Vec<Fill>,
    },
    /// Rows given as VALUES, each cell a literal or the column's DEFAULT.
    Filled(Vec<Vec<Fill>>),
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum Statement {
    Begin,
    Commit,
    Rollback,
    Insert {
        table: usize,
        source: Source,
    },
    /// The question that finds the rows, every column of the table in its order, and the values set.
    Update {
        table: usize,
        find: String,
        sets: Vec<(usize, Datum)>,
    },
    Delete {
        table: usize,
        find: String,
    },
    /// An INSERT, UPDATE or DELETE that returns rows of what it changed: the write, the name its
    /// table is read by, and its RETURNING list, read over the rows changed, each with its values
    /// before (`old`) and after (`new`) as PostgreSQL 18 reads them.
    Returning {
        write: Box<Statement>,
        alias: String,
        items: Vec<sqlparser::ast::SelectItem>,
    },
    /// An INSERT … ON CONFLICT: the INSERT, the name its table is read by, the order a proposed row
    /// is looked up on (any unique one where none is named), and DO UPDATE's columns set and its
    /// WHERE, none for DO NOTHING.
    OnConflict {
        insert: Box<Statement>,
        alias: String,
        arbiter: Option<usize>,
        update: Option<(Vec<(usize, Fill)>, Option<String>)>,
    },
    /// A write through a view, as the write to the table under it, and the conditions its new rows
    /// must meet, each with the view whose CHECK OPTION it is.
    Checked {
        write: Box<Statement>,
        checks: Vec<(String, String)>,
    },
    /// A question whose WITH holds writes: each write by the name its WITH query takes, and the
    /// question. Every write finds its rows on the statement's one snapshot, none seeing another's,
    /// the question reading that snapshot and the rows each write returns.
    Writes {
        writes: Vec<(String, Statement)>,
        question: String,
    },
    /// A table, an index or a table's order declared: its DDL, and what it declares.
    Declare {
        ddl: String,
        kind: &'static str,
    },
    /// An UPDATE: the question that finds the rows, every column of the table in its order and
    /// after them each value an expression sets, and each column set, to a value written out, to
    /// its DEFAULT, or to the finder's answer after the row's columns.
    Set {
        table: usize,
        find: String,
        sets: Vec<(usize, Fill)>,
    },
    /// SET CONSTRAINTS: the foreign keys named, or ALL where none are, deferred to the commit or
    /// checked at each statement's end until the transaction ends.
    Constraints {
        names: Option<Vec<String>>,
        deferred: bool,
    },
    /// A setting set to the value the snail answers under, which changes nothing: its name.
    Setting {
        name: String,
    },
    /// What a transaction runs as, its isolation level, READ ONLY or READ WRITE, DEFERRABLE or not:
    /// named by the BEGIN or START TRANSACTION that begins it, by SET TRANSACTION for the transaction
    /// begun, or by SET SESSION CHARACTERISTICS AS TRANSACTION for the transactions after.
    Isolation {
        modes: Modes,
        at: Isolated,
    },
    /// A question asked among a writer's statements: in a transaction, it reads the transaction's own
    /// rows; with a locking clause, it locks the rows it returns.
    Question {
        question: String,
    },
    /// A statement of the session's own: a savepoint's, an end that chains the next transaction,
    /// LOCK TABLE, a setting's, a prepared statement's, a cursor's, COPY or EXPLAIN.
    Session(super::session::Said),
}

/// The isolation levels the snail runs: READ COMMITTED, which PostgreSQL also runs READ UNCOMMITTED
/// as and holds as a level of its own, REPEATABLE READ and SERIALIZABLE.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolation {
    ReadUncommitted,
    #[default]
    ReadCommitted,
    RepeatableRead,
    Serializable,
}

/// What a statement names of a transaction's modes, each the last it names.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Modes {
    pub level: Option<Isolation>,
    pub read_only: Option<bool>,
    pub deferrable: Option<bool>,
}

/// Where an isolation level is named.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Isolated {
    Begin,
    Transaction,
    Session,
}

/// What a transaction runs as.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Mode {
    level: Isolation,
    read_only: bool,
    deferrable: bool,
}

impl Mode {
    fn with(mut self, modes: Modes) -> Mode {
        self.level = modes.level.unwrap_or(self.level);
        self.read_only = modes.read_only.unwrap_or(self.read_only);
        self.deferrable = modes.deferrable.unwrap_or(self.deferrable);
        self
    }
}

/// What a writer's transactions run as: the session's, the transaction's, and whether a statement
/// of the transaction has read, as PostgreSQL's first snapshot of a transaction is taken; and the
/// session's when the transaction began, which a ROLLBACK puts back.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Levels {
    session: Mode,
    transaction: Mode,
    read: bool,
    session_at_begin: Mode,
}

impl Levels {
    /// A transaction begun, as named or else as the session's.
    pub(crate) fn begin(&mut self, modes: Modes) {
        self.transaction = self.session.with(modes);
        self.read = false;
        self.session_at_begin = self.session;
    }

    /// A statement of the transaction that reads, or writes.
    pub(crate) fn read(&mut self) {
        self.read = true;
    }

    /// Modes named outside BEGIN, `begun` whether a transaction is: SET TRANSACTION's, which outside
    /// a transaction does nothing, and inside one, after its first read, changes neither its level
    /// nor READ ONLY to READ WRITE, and takes no [NOT] DEFERRABLE; or SET SESSION CHARACTERISTICS's.
    /// In a savepoint, `sub`, as in a subtransaction of PostgreSQL's, neither the level nor [NOT]
    /// DEFERRABLE is set, nor READ WRITE in a transaction READ ONLY.
    pub(crate) fn set(&mut self, modes: Modes, at: Isolated, begun: bool, sub: bool) -> Result<()> {
        match at {
            Isolated::Transaction if begun => {
                let level = modes.level.is_some_and(|l| l != self.transaction.level);
                if level && !self.read && sub {
                    return Err(Error::new(
                        "SET TRANSACTION ISOLATION LEVEL must not be called in a subtransaction",
                    ));
                }
                if modes.deferrable.is_some() && sub {
                    return Err(Error::new(
                        "SET TRANSACTION [NOT] DEFERRABLE cannot be called within a subtransaction",
                    ));
                }
                if modes.read_only == Some(false) && self.transaction.read_only && sub {
                    return Err(Error::new(
                        "cannot set transaction read-write mode inside a read-only transaction",
                    ));
                }
                if self.read {
                    if level {
                        return Err(Error::new(
                            "SET TRANSACTION ISOLATION LEVEL must be called before any query",
                        ));
                    }
                    if modes.read_only == Some(false) && self.transaction.read_only {
                        return Err(Error::new(
                            "transaction read-write mode must be set before any query",
                        ));
                    }
                    if modes.deferrable.is_some() {
                        return Err(Error::new(
                            "SET TRANSACTION [NOT] DEFERRABLE must be called before any query",
                        ));
                    }
                }
                self.transaction = self.transaction.with(modes);
            }
            Isolated::Session => self.session = self.session.with(modes),
            _ => {}
        }
        Ok(())
    }

    fn of(&self, begun: bool) -> Mode {
        match begun {
            true => self.transaction,
            false => self.session,
        }
    }

    /// Whether a statement reads one snapshot for its transaction's life, as REPEATABLE READ and
    /// SERIALIZABLE do: in a transaction, at the transaction's level; alone, at the session's.
    pub(crate) fn repeatable(&self, begun: bool) -> bool {
        matches!(
            self.of(begun).level,
            Isolation::RepeatableRead | Isolation::Serializable
        )
    }

    /// Whether a statement runs at SERIALIZABLE.
    pub(crate) fn serializable(&self, begun: bool) -> bool {
        self.of(begun).level == Isolation::Serializable
    }

    /// Whether a statement runs READ ONLY.
    pub(crate) fn read_only(&self, begun: bool) -> bool {
        self.of(begun).read_only
    }

    /// Whether a statement runs DEFERRABLE.
    pub(crate) fn deferrable(&self, begun: bool) -> bool {
        self.of(begun).deferrable
    }

    /// The transaction ended: committed, or rolled back, its SET SESSION undone.
    pub(crate) fn end(&mut self, committed: bool) {
        if !committed {
            self.session = self.session_at_begin;
        }
    }

    /// The modes as a savepoint kept them, put back: what SET TRANSACTION and SET SESSION
    /// CHARACTERISTICS named since is undone, and the transaction's first read stays taken.
    pub(crate) fn back_to(&mut self, kept: &Levels) {
        self.session = kept.session;
        self.transaction = kept.transaction;
    }

    /// RESET ALL: the session's modes back at their defaults.
    pub(crate) fn reset_session(&mut self) {
        self.session = Mode::default();
    }

    /// The modes the transaction under way runs as, as BEGIN would name them: what COMMIT AND
    /// CHAIN and ROLLBACK AND CHAIN begin the next with.
    pub(crate) fn chained(&self) -> Modes {
        Modes {
            level: Some(self.transaction.level),
            read_only: Some(self.transaction.read_only),
            deferrable: Some(self.transaction.deferrable),
        }
    }

    /// What `SHOW` gives for the transaction's modes and the session's defaults, by the names
    /// PostgreSQL's settings take: none for any other name.
    pub(crate) fn shown(&self, name: &str, begun: bool) -> Option<&'static str> {
        let level = |l: Isolation| match l {
            Isolation::ReadUncommitted => "read uncommitted",
            Isolation::ReadCommitted => "read committed",
            Isolation::RepeatableRead => "repeatable read",
            Isolation::Serializable => "serializable",
        };
        let truth = |b: bool| if b { "on" } else { "off" };
        let now = self.of(begun);
        Some(match name {
            "transaction_isolation" => level(now.level),
            "transaction_read_only" => truth(now.read_only),
            "transaction_deferrable" => truth(now.deferrable),
            "default_transaction_isolation" => level(self.session.level),
            "default_transaction_read_only" => truth(self.session.read_only),
            "default_transaction_deferrable" => truth(self.session.deferrable),
            _ => return None,
        })
    }
}

impl Statement {
    pub fn kind(&self) -> &'static str {
        match self {
            Statement::Begin => "BEGIN",
            Statement::Isolation {
                at: Isolated::Begin,
                ..
            } => "BEGIN",
            Statement::Isolation { .. } => "SET",
            Statement::Commit => "COMMIT",
            Statement::Rollback => "ROLLBACK",
            Statement::Insert { .. } => "INSERT",
            Statement::Update { .. } => "UPDATE",
            Statement::Delete { .. } => "DELETE",
            Statement::Declare { kind, .. } => kind,
            Statement::Set { .. } => "UPDATE",
            Statement::Constraints { .. } => "SET CONSTRAINTS",
            Statement::Setting { .. } => "SET",
            Statement::Question { .. } => "SELECT",
            Statement::Session(s) => s.kind(),
            Statement::Returning { write, .. } => write.kind(),
            Statement::Writes { .. } => "SELECT",
            Statement::OnConflict { .. } => "INSERT",
            Statement::Checked { write, .. } => write.kind(),
        }
    }

    /// The write itself, its RETURNING and its ON CONFLICT taken off; any other statement as it is.
    pub fn written(&self) -> &Statement {
        match self {
            Statement::Returning { write, .. } | Statement::Checked { write, .. } => write.written(),
            Statement::OnConflict { insert, .. } => insert,
            st => st,
        }
    }
}

/// The BEGIN that begins a transaction of `modes`.
pub(crate) fn begin_text(modes: Modes) -> String {
    let level = match modes.level.unwrap_or_default() {
        Isolation::ReadUncommitted => "READ UNCOMMITTED",
        Isolation::ReadCommitted => "READ COMMITTED",
        Isolation::RepeatableRead => "REPEATABLE READ",
        Isolation::Serializable => "SERIALIZABLE",
    };
    let rw = match modes.read_only {
        Some(true) => "READ ONLY",
        _ => "READ WRITE",
    };
    let deferrable = match modes.deferrable {
        Some(true) => "DEFERRABLE",
        _ => "NOT DEFERRABLE",
    };
    format!("BEGIN ISOLATION LEVEL {level}, {rw}, {deferrable}")
}

fn refused(what: &str) -> Error {
    Error::new(format!("{what} is not written"))
}

/// BEGIN, START TRANSACTION, SET TRANSACTION and SET SESSION CHARACTERISTICS with their modes, read
/// off their words as PostgreSQL's grammar takes them, sqlparser reading no [NOT] DEFERRABLE: none
/// where the words are no such statement.
fn transaction_statement(tokens: &[sqlparser::tokenizer::Token]) -> Result<Option<Statement>> {
    use sqlparser::tokenizer::Token;
    let mut words = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .peekable();
    let keyword = |t: Option<&&Token>| match t.copied() {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_uppercase(),
        _ => String::new(),
    };
    let at = match keyword(words.peek()).as_str() {
        "BEGIN" => {
            words.next();
            if matches!(keyword(words.peek()).as_str(), "WORK" | "TRANSACTION") {
                words.next();
            }
            Isolated::Begin
        }
        "START" => {
            words.next();
            if keyword(words.peek()) != "TRANSACTION" {
                return Ok(None);
            }
            words.next();
            Isolated::Begin
        }
        "SET" => {
            words.next();
            if matches!(keyword(words.peek()).as_str(), "LOCAL" | "SESSION") {
                let mut after = words.clone();
                after.next();
                if matches!(keyword(after.peek()).as_str(), "TRANSACTION" | "SESSION") {
                    words = after;
                }
            }
            match keyword(words.peek()).as_str() {
                "TRANSACTION" => {
                    words.next();
                    if keyword(words.peek()) == "SNAPSHOT" {
                        return Ok(None);
                    }
                    Isolated::Transaction
                }
                "SESSION" => {
                    words.next();
                    if keyword(words.peek()) != "CHARACTERISTICS" {
                        return Ok(None);
                    }
                    words.next();
                    for w in ["AS", "TRANSACTION"] {
                        if keyword(words.peek()) != w {
                            return Ok(None);
                        }
                        words.next();
                    }
                    Isolated::Session
                }
                _ => return Ok(None),
            }
        }
        _ => return Ok(None),
    };
    let syntax = |t: Option<&Token>| {
        Error::new(match t {
            Some(t) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".to_string(),
        })
    };
    let mut modes = Modes::default();
    let mut any = false;
    loop {
        let mut t = words.next();
        match t {
            None | Some(Token::EOF) => break,
            Some(Token::SemiColon) => {
                if words.any(|t| !matches!(t, Token::EOF)) {
                    return Err(Error::new("one statement at a time"));
                }
                break;
            }
            Some(Token::Comma) if any => t = words.next(),
            _ => {}
        }
        read_mode(t, &mut words, &mut modes, &syntax)?;
        any = true;
    }
    if !any && at != Isolated::Begin {
        return Err(syntax(None));
    }
    Ok(Some(match (any, at) {
        (false, _) => Statement::Begin,
        (true, at) => Statement::Isolation { modes, at },
    }))
}

/// One of a transaction's modes, its first word `first`.
fn read_mode<'a>(
    first: Option<&'a sqlparser::tokenizer::Token>,
    words: &mut (impl Iterator<Item = &'a sqlparser::tokenizer::Token> + Clone),
    modes: &mut Modes,
    syntax: &dyn Fn(Option<&sqlparser::tokenizer::Token>) -> Error,
) -> Result<()> {
    use sqlparser::tokenizer::Token;
    let keyword = |t: Option<&Token>| match t {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_uppercase(),
        _ => String::new(),
    };
    match keyword(first).as_str() {
        "ISOLATION" => {
            let t = words.next();
            if keyword(t) != "LEVEL" {
                return Err(syntax(t));
            }
            let t = words.next();
            modes.level = Some(match keyword(t).as_str() {
                "SERIALIZABLE" => Isolation::Serializable,
                "REPEATABLE" => {
                    let r = words.next();
                    if keyword(r) != "READ" {
                        return Err(syntax(r));
                    }
                    Isolation::RepeatableRead
                }
                "READ" => {
                    let r = words.next();
                    match keyword(r).as_str() {
                        "COMMITTED" => Isolation::ReadCommitted,
                        "UNCOMMITTED" => Isolation::ReadUncommitted,
                        _ => return Err(syntax(r)),
                    }
                }
                _ => return Err(syntax(t)),
            });
        }
        "READ" => {
            let t = words.next();
            modes.read_only = Some(match keyword(t).as_str() {
                "ONLY" => true,
                "WRITE" => false,
                _ => return Err(syntax(t)),
            });
        }
        "DEFERRABLE" => modes.deferrable = Some(true),
        "NOT" => {
            let t = words.next();
            if keyword(t) != "DEFERRABLE" {
                return Err(syntax(t));
            }
            modes.deferrable = Some(false);
        }
        _ => return Err(syntax(first)),
    }
    Ok(())
}

fn table_index(catalog: &Catalog, name: &ObjectName) -> Result<usize> {
    let name = object_name(name);
    catalog
        .tables
        .iter()
        .position(|t| t.name == name)
        .ok_or_else(|| Error::new(format!("{name} is not in the snail")))
}

fn column_index(table: &Table, name: &ObjectName) -> Result<usize> {
    let last = match name.0.last() {
        Some(ObjectNamePart::Identifier(i)) => ident(i),
        _ => return Err(Error::new(format!("{name} is not a column"))),
    };
    table
        .columns
        .iter()
        .position(|c| c.name == last)
        .ok_or_else(|| Error::new(format!(
            "column \"{last}\" of relation \"{}\" does not exist",
            table.name.rsplit('.').next().unwrap_or(&table.name)
        )))
}

/// A geometric literal cast to another geometric type inside its cast to `to`, cast as PostgreSQL
/// casts it, `CAST(CAST('(0,0),(1,1)' AS box) AS polygon)` a box's polygon; none where every cast
/// names `to`. Refused where a cast names a type no geometric value is cast to.
fn geo_cast_through(e: &Expr, to: crate::geo::GeoKind) -> Result<Option<crate::geo::Geo>> {
    let mut kinds = Vec::new();
    let mut x = e;
    loop {
        match x {
            Expr::Nested(inner) => x = inner,
            Expr::Cast {
                expr, data_type, ..
            } => {
                let name = data_type.to_string().to_lowercase();
                let name = name.strip_prefix("pg_catalog.").unwrap_or(&name);
                kinds.push(crate::geo::GeoKind::of_name(name).ok_or_else(|| {
                    Error::new(format!(
                        "{e}: a cast to {name} is not a value of {}",
                        to.name()
                    ))
                })?);
                x = expr;
            }
            _ => break,
        }
    }
    if kinds.iter().all(|&k| k == to) {
        return Ok(None);
    }
    let mut g = crate::geo::geo_in(*kinds.last().unwrap_or(&to), &text_literal(x)?)?;
    for &k in kinds.iter().rev().skip(1).chain(std::iter::once(&to)) {
        g = crate::geo::geo_cast(&g, k).ok_or_else(|| {
            Error::new(format!(
                "cannot cast type {} to {}",
                g.kind().name(),
                k.name()
            ))
        })??;
    }
    Ok(Some(g))
}

/// A literal written with its casts, read as PostgreSQL reads it: by the innermost cast's type, cast
/// outward by each in turn, then assigned to column `c` from the outermost cast's type. None where
/// it has no cast, or where every cast names the column's own type and that type cuts nothing a
/// cast would cut, so that the column's input reads it alike.
fn cast_through(e: &Expr, table: &Table, c: usize) -> Result<Option<Datum>> {
    let col = &table.columns[c];
    let own = |dt: &sqlparser::ast::DataType| {
        let name = dt.to_string().to_lowercase();
        crate::catalog::held_type(dt).is_ok_and(|ty| ty.with_monetary(table.monetary) == col.ty)
            || col.domain.as_deref() == Some(name.as_str())
            || (matches!(col.ty, ColumnType::Composed(_)) && col.ty.pg_name() == name)
    };
    let mut outermost = None;
    let mut every_own = true;
    let mut x = e;
    loop {
        match x {
            Expr::Nested(inner) => x = inner,
            // A minus sign before a cast negates the value the cast gives.
            Expr::UnaryOp {
                op: UnaryOperator::Minus,
                expr,
            } => {
                every_own = false;
                x = expr;
            }
            Expr::Cast {
                expr, data_type, ..
            } => {
                outermost.get_or_insert(data_type);
                every_own &= own(data_type);
                x = expr;
            }
            _ => break,
        }
    }
    let Some(to) = outermost else {
        return Ok(None);
    };
    let cuts = matches!(
        col.ty,
        ColumnType::Text(Some(_))
            | ColumnType::Bpchar(Some(_))
            | ColumnType::Bit(_)
            | ColumnType::Varbit(Some(_))
    );
    if every_own && !cuts {
        return Ok(None);
    }
    let v = super::pipeline::eval_on(
        e,
        &[],
        &[],
        table.release,
        &super::pipeline::Casings::under(table),
    )?;
    let held = crate::catalog::held_type(to)
        .ok()
        .or_else(|| crate::composed::resolve(to));
    let Some(from) = held.map(|ty| ty.with_monetary(table.monetary)) else {
        return super::fill::assign_value(v, col.ty, &col.name).map(Some);
    };
    if !crate::catalog::assignable(from, col.ty) {
        // An array is assigned element by element, each by its element's assignment: read from its
        // text by the column's input.
        if elements_assignable(from, col.ty) {
            return match v.text() {
                Some(t) => crate::composed::scalar_in(col.ty, &t).map(Some),
                None => Ok(Some(Datum::Null)),
            };
        }
        return Err(Error::new(format!(
            "column \"{}\" is of type {} but expression is of type {}",
            col.name,
            col.ty.pg_name(),
            from.pg_name()
        )));
    }
    let d = super::fill::assign_value(v, from, &col.name)?;
    super::fill::assign(d, from, col.ty, &col.name).map(Some)
}

/// Whether two array types' elements, or theirs in turn, are assigned one to the other.
fn elements_assignable(from: ColumnType, to: ColumnType) -> bool {
    if crate::composed::is_vector(to) {
        return false;
    }
    let element = |ty: ColumnType| match ty {
        ColumnType::Composed(r) => match r.def() {
            crate::composed::Composed::Array { element, .. }
            | crate::composed::Composed::Vector { element } => Some(*element),
            _ => None,
        },
        _ => None,
    };
    match (element(from), element(to)) {
        (Some(a), Some(b)) => crate::catalog::assignable(a, b) || elements_assignable(a, b),
        _ => false,
    }
}

/// An expression with its casts taken off.
fn uncast(e: &Expr) -> &Expr {
    match e {
        Expr::Cast { expr, .. } | Expr::Nested(expr) => uncast(expr),
        other => other,
    }
}

/// Whether an expression is the keyword DEFAULT, which takes the column's DEFAULT.
fn is_default(e: &Expr) -> bool {
    matches!(e, Expr::Identifier(i) if i.quote_style.is_none() && i.value.eq_ignore_ascii_case("default"))
}

/// Whether an expression is a literal, a value written out.
pub(crate) fn is_literal(e: &Expr) -> bool {
    match uncast(e) {
        Expr::Value(_) => true,
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => is_literal(expr),
        _ => false,
    }
}

/// A number written out, a minus sign before it or not, as its text.
pub(crate) fn number_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => Some(s.clone()),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => number_text(expr).map(|s| format!("-{s}")),
        Expr::Nested(x) => number_text(x),
        _ => None,
    }
}

/// A number written out as PostgreSQL types it: an integer, a bigint past an integer's range, a
/// numeric where it has a point or an exponent or passes a bigint's range; with its value.
pub(super) fn number_datum(e: &Expr) -> Option<(Datum, ColumnType)> {
    let text = match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) => s.clone(),
            _ => return None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(v) => match &v.value {
                Value::Number(s, _) => format!("-{s}"),
                _ => return None,
            },
            _ => return None,
        },
        Expr::Nested(x) => return number_datum(x),
        _ => return None,
    };
    if let Ok(v) = text.parse::<i64>() {
        return Some((
            Datum::Int(v),
            if i32::try_from(v).is_ok() {
                ColumnType::Int4
            } else {
                ColumnType::Int8
            },
        ));
    }
    let n = crate::numeric::Numeric::parse(&text).ok()?;
    Some((Datum::Text(n.to_string()), ColumnType::NumericText(None)))
}

/// A number written out, or a quoted literal, as the text a numeric's input reads.
pub(crate) fn numeric_literal_text(e: &Expr) -> Option<String> {
    match e {
        Expr::Value(v) => match &v.value {
            Value::Number(s, _) | Value::SingleQuotedString(s) => Some(s.clone()),
            _ => None,
        },
        Expr::UnaryOp {
            op: UnaryOperator::Minus,
            expr,
        } => match expr.as_ref() {
            Expr::Value(v) => match &v.value {
                Value::Number(s, _) => Some(format!("-{s}")),
                _ => None,
            },
            _ => None,
        },
        Expr::Nested(x) => numeric_literal_text(x),
        _ => None,
    }
}

/// A literal as a value of column `c`.
pub fn datum(e: &Expr, table: &Table, c: usize) -> Result<Datum> {
    if let ColumnType::Geometric(k) = table.columns[c].ty {
        if let Some(g) = geo_cast_through(e, k)? {
            return Ok(Datum::Bytes(crate::geo::geo_bytes(&g)));
        }
    } else if let Some(d) = cast_through(e, table, c)? {
        return Ok(d);
    }
    let e = uncast(e);
    if let Expr::Value(v) = e {
        if matches!(v.value, Value::Null) {
            return Ok(Datum::Null);
        }
    }
    let col = &table.columns[c];
    let wrong = || Error::new(format!("{e} is not a value of {}.{}", table.name, col.name));
    Ok(match col.ty {
        ty @ ColumnType::Composed(_) => crate::composed::literal(ty, e)?,
        ColumnType::Boolean => Datum::Int(truth_literal(e).map_err(|_| wrong())?),
        ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 => {
            // A quoted literal is read by the integer's input.
            let quoted = text_literal(e)
                .ok()
                .map(|s| crate::composed::int_in(&s, col.ty.integer_bits()))
                .transpose()?;
            let (lo, hi, name) = match col.ty {
                ColumnType::Int2 => (i64::from(i16::MIN), i64::from(i16::MAX), "smallint"),
                ColumnType::Int4 => (i64::from(i32::MIN), i64::from(i32::MAX), "integer"),
                _ => (i64::MIN, i64::MAX, "bigint"),
            };
            // A number is cast to the column as its assignment casts it: an integer past the
            // column's range, or a numeric (a point, an exponent, past a bigint) rounded half away
            // from zero, refused as `smallint out of range` and the like.
            let v = match quoted.or_else(|| number(e)) {
                Some(v) => v,
                None => crate::numeric::Numeric::parse(&number_text(e).ok_or_else(wrong)?)
                    .map_err(|_| wrong())?
                    .to_integer(lo, hi, name)?,
            };
            if v < lo || v > hi {
                return Err(Error::new(format!("{name} out of range")));
            }
            Datum::Int(v)
        }
        ColumnType::Timestamptz => Datum::Int(instant(&text_literal(e).map_err(|_| wrong())?)?),
        ColumnType::TimestamptzRounded(p) => Datum::Int(crate::times::timestamptz_in(
            &text_literal(e).map_err(|_| wrong())?,
            Some(u32::from(p)),
            crate::scalars::transaction_clock(),
        )?),
        ColumnType::Interval { range, precision } => Datum::Interval(crate::times::interval_in(
            &text_literal(e).map_err(|_| wrong())?,
            range,
            precision.map(u32::from),
        )?),
        ColumnType::Uuid => Datum::Wide(crate::scalars::uuid_parse(
            &text_literal(e).map_err(|_| wrong())?,
        )?),
        ColumnType::Float4 | ColumnType::Float8 => {
            let wide = col.ty == ColumnType::Float8;
            let text = match text_literal(e) {
                Ok(s) => s,
                // A number past a bigint, or with a point or an exponent, is a numeric, cast to a
                // float through its text as numeric_float4 and numeric_float8 cast it.
                Err(_) => {
                    let t = number_text(e).ok_or_else(wrong)?;
                    match number(e) {
                        Some(_) => t,
                        None => crate::numeric::Numeric::parse(&t).map_or(t, |n| n.to_string()),
                    }
                }
            };
            let bits = crate::scalars::float_parse(&text, wide)?;
            if wide {
                Datum::Float8(bits)
            } else {
                Datum::Float4(bits as u32)
            }
        }
        ty @ (ColumnType::Oid
        | ColumnType::Reg(..)
        | ColumnType::Oid8
        | ColumnType::Xid
        | ColumnType::Cid
        | ColumnType::Xid8
        | ColumnType::PgLsn
        | ColumnType::Char
        | ColumnType::Money(_)
        | ColumnType::Macaddr
        | ColumnType::Macaddr8
        | ColumnType::Tid
        | ColumnType::Date
        | ColumnType::Time(_)
        | ColumnType::Timetz(_)
        | ColumnType::Timestamp(_)) => match text_literal(e) {
            Ok(s) => Datum::Int(crate::scalars::scalar_parse(ty, &s)?),
            Err(_) => {
                let (d, from) = number_datum(e).ok_or_else(wrong)?;
                super::fill::assign(d, from, ty, &col.name)?
            }
        },
        // A literal is held to its column's length as PostgreSQL plans the statement, before any
        // DEFAULT draws a value: its excess taken off where it is only spaces, else refused.
        ColumnType::Text(n) => super::fill::assign(
            Datum::Text(text_literal(e).map_err(|_| wrong())?),
            ColumnType::Text(None),
            ColumnType::Text(n),
            &col.name,
        )?,
        ColumnType::Bytes => Datum::Bytes(crate::strings::bytea_in(
            &text_literal(e).map_err(|_| wrong())?,
        )?),
        // A literal is read by its type's input, under the column's modifier: a bpchar padded to its
        // length or its excess blanks taken off, a name cut to 63 bytes, a bit string held to its
        // length.
        ColumnType::Bpchar(n) => Datum::Text(crate::strings::bpchar_in(
            &text_literal(e).map_err(|_| wrong())?,
            n,
            false,
        )?),
        ColumnType::Name => Datum::Text(crate::strings::name_in(
            &text_literal(e).map_err(|_| wrong())?,
        )),
        ColumnType::Bit(n) => Datum::Bytes(crate::strings::bits_parse(
            &super::question::bit_literal(e)
                .map_or_else(|| text_literal(e), Ok)
                .map_err(|_| wrong())?,
            Some(n),
            false,
        )?),
        ColumnType::Varbit(n) => Datum::Bytes(crate::strings::bits_parse(
            &super::question::bit_literal(e)
                .map_or_else(|| text_literal(e), Ok)
                .map_err(|_| wrong())?,
            n,
            true,
        )?),
        ty @ (ColumnType::Numeric { .. } | ColumnType::NumericText(_)) => {
            let text = numeric_literal_text(e).ok_or_else(wrong)?;
            super::fill::numeric_datum(crate::numeric::Numeric::parse(&text)?, ty)?
        }
        // A literal is read by its type's input into the text the snail holds.
        ColumnType::Textual(t) => Datum::Text(crate::textual::text_in(
            t,
            &text_literal(e).map_err(|_| wrong())?,
        )?),
        ColumnType::Internal(t) => return Err(t.refusal()),
        ColumnType::Geometric(k) => Datum::Bytes(crate::geo::geo_bytes(&crate::geo::geo_in(
            k,
            &text_literal(e).map_err(|_| wrong())?,
        )?)),
    })
}

/// The question that finds a table's rows, every column it stores in the table's order.
fn finder(table: &Table, relation: &TableWithJoins, selection: Option<&Expr>) -> String {
    // Its stored columns: a virtual one is computed again from them.
    let columns = table
        .columns
        .iter()
        .filter(|c| !c.is_virtual())
        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(", ");
    match selection {
        Some(w) => format!("SELECT {columns} FROM {relation} WHERE {w}"),
        None => format!("SELECT {columns} FROM {relation}"),
    }
}

/// Whether a WITH query writes.
pub(crate) fn writes_query(q: &Query) -> bool {
    matches!(
        q.body.as_ref(),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_)
    )
}

/// A write that returns rows where it has a RETURNING list, its table read by `alias`.
fn returning(write: Statement, alias: String, items: &Option<Vec<SelectItem>>) -> Statement {
    match items {
        Some(items) => Statement::Returning {
            write: Box::new(write),
            alias,
            items: items.clone(),
        },
        None => write,
    }
}

/// The name a write's table is read by: its alias, or its own name without its schema.
fn read_as(t: &TableWithJoins) -> String {
    match &t.relation {
        TableFactor::Table {
            alias: Some(a), ..
        } => ident(&a.name),
        TableFactor::Table { name, .. } => {
            let n = object_name(name);
            n.rsplit('.').next().unwrap_or(&n).to_string()
        }
        _ => String::new(),
    }
}

/// The question that finds a write's rows joined with the relations its FROM or USING lists: every
/// column the table stores, read by the name it reads the table by, then each value `asked`.
fn joined_finder(
    table: &Table,
    relation: &TableWithJoins,
    from: &[TableWithJoins],
    asked: &[String],
    selection: Option<&Expr>,
) -> String {
    let by = format!("\"{}\"", read_as(relation).replace('"', "\"\""));
    let columns = table
        .columns
        .iter()
        .filter(|c| !c.is_virtual())
        .map(|c| format!("{by}.\"{}\"", c.name.replace('"', "\"\"")))
        .chain(asked.iter().cloned())
        .collect::<Vec<_>>()
        .join(", ");
    let relations = std::iter::once(relation.to_string())
        .chain(from.iter().map(|f| f.to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    match selection {
        Some(w) => format!("SELECT {columns} FROM {relations} WHERE {w}"),
        None => format!("SELECT {columns} FROM {relations}"),
    }
}

/// The one table a statement names, read alone.
fn one_table(catalog: &Catalog, t: &TableWithJoins) -> Result<usize> {
    if !t.joins.is_empty() {
        return Err(refused("a write that joins"));
    }
    match &t.relation {
        TableFactor::Table { name, .. } => table_index(catalog, name),
        _ => Err(Error::new("a write names a table")),
    }
}

/// The columns an INSERT's rows of `width` values fill, as PostgreSQL's parser matches them: the
/// columns listed, each given one; with none listed, the table's first.
fn filled(columns: &[usize], width: usize, listed: bool) -> Result<Vec<usize>> {
    if width > columns.len() {
        return Err(Error::new("INSERT has more expressions than target columns"));
    }
    if width < columns.len() && listed {
        return Err(Error::new("INSERT has more target columns than expressions"));
    }
    Ok(columns[..width].to_vec())
}

fn insert(catalog: &Catalog, i: &Insert) -> Result<Statement> {
    let ti = match &i.table {
        TableObject::TableName(n) => table_index(catalog, n)?,
        _ => return Err(Error::new("an INSERT names a table")),
    };
    let table = &catalog.tables[ti];
    let columns: Vec<usize> = if i.columns.is_empty() {
        (0..table.columns.len()).collect()
    } else {
        i.columns
            .iter()
            .map(|n| column_index(table, n))
            .collect::<Result<_>>()?
    };
    for (k, c) in columns.iter().enumerate() {
        if columns[..k].contains(c) {
            return Err(Error::new(format!(
                "{} is named twice",
                table.columns[*c].name
            )));
        }
    }
    let q: &Query = i
        .source
        .as_deref()
        .ok_or_else(|| Error::new("an INSERT gives its rows"))?;
    if q.with.is_some() {
        return Err(refused("a write inside WITH"));
    }
    let source = match q.body.as_ref() {
        SetExpr::Values(v) => {
            let width = v.rows.first().map_or(0, |r| r.content.len());
            if v.rows.iter().any(|r| r.content.len() != width) {
                return Err(Error::new("VALUES lists must all be the same length"));
            }
            let columns = filled(&columns, width, !i.columns.is_empty())?;
            let mut rows = Vec::with_capacity(v.rows.len());
            for r in &v.rows {
                let mut row = vec![Fill::Default; table.columns.len()];
                for (&c, e) in columns.iter().zip(&r.content) {
                    row[c] = if is_default(e) {
                        Fill::Default
                    } else {
                        Fill::Literal(datum(e, table, c)?)
                    };
                }
                rows.push(row);
            }
            Source::Filled(rows)
        }
        SetExpr::Select(s) => {
            let columns = filled(&columns, s.projection.len(), !i.columns.is_empty())?;
            let mut fill = vec![Fill::Default; table.columns.len()];
            let mut kept = Vec::new();
            for (&c, item) in columns.iter().zip(&s.projection) {
                let e = match item {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                    _ => return Err(Error::new("name each column the INSERT's question returns")),
                };
                if is_literal(e) || crate::composed::constructs(table.columns[c].ty, e) {
                    fill[c] = Fill::Literal(datum(e, table, c)?);
                } else {
                    fill[c] = Fill::Answer(kept.len());
                    kept.push(item.clone());
                }
            }
            if kept.is_empty() {
                return Err(Error::new(
                    "an INSERT's question returns a column of the table it reads",
                ));
            }
            let mut reduced = q.clone();
            if let SetExpr::Select(s) = reduced.body.as_mut() {
                s.projection = kept;
            }
            Source::Select {
                question: reduced.to_string(),
                fill,
            }
        }
        _ => return Err(Error::new("an INSERT's rows are VALUES or a SELECT")),
    };
    let st = Statement::Insert { table: ti, source };
    let Some(on) = &i.on else {
        return Ok(st);
    };
    let sqlparser::ast::OnInsert::OnConflict(c) = on else {
        return Err(refused("ON DUPLICATE KEY UPDATE"));
    };
    use sqlparser::ast::{ConflictTarget, OnConflictAction};
    let update = match &c.action {
        OnConflictAction::DoNothing => None,
        OnConflictAction::DoUpdate(d) => Some((
            sets_of(table, &d.assignments)?,
            d.selection.as_ref().map(|e| e.to_string()),
        )),
    };
    let bare = table.name.rsplit('.').next().unwrap_or(&table.name);
    // The order a proposed row is looked up on, as PostgreSQL's parser infers it: the unique one,
    // no partial one, whose columns are the target's, or the constraint named; any unique one
    // where DO NOTHING names none.
    let arbiter = match &c.conflict_target {
        None if update.is_some() => {
            return Err(Error::new(
                "ON CONFLICT DO UPDATE requires inference specification or constraint name",
            ))
        }
        None => None,
        Some(ConflictTarget::Columns(cols)) => {
            let mut want = cols
                .iter()
                .map(|n| column_index(table, &ObjectName(vec![ObjectNamePart::Identifier(n.clone())])))
                .collect::<Result<Vec<_>>>()?;
            want.sort_unstable();
            want.dedup();
            let found = table.orders.iter().position(|o| {
                let mut have: Vec<usize> = o.levels.iter().map(|l| l.column()).collect();
                have.sort_unstable();
                o.unique && o.predicate.is_none() && have == want
            });
            Some(found.ok_or_else(|| {
                Error::new(
                    "there is no unique or exclusion constraint matching the ON CONFLICT \
                     specification",
                )
            })?)
        }
        Some(ConflictTarget::OnConstraint(n)) => {
            let n = object_name(n);
            Some(
                table
                    .orders
                    .iter()
                    .position(|o| o.unique && o.name == n)
                    .ok_or_else(|| {
                        Error::new(format!(
                            "constraint \"{n}\" for table \"{bare}\" does not exist"
                        ))
                    })?,
            )
        }
    };
    let alias = match &i.table_alias {
        Some(a) => ident(&a.alias),
        None => bare.to_string(),
    };
    Ok(Statement::OnConflict {
        insert: Box::new(st),
        alias,
        arbiter,
        update,
    })
}

/// An UPDATE's SET, each column to its DEFAULT, a literal, or an expression read on the row.
fn sets_of(
    table: &Table,
    assignments: &[sqlparser::ast::Assignment],
) -> Result<Vec<(usize, Fill)>> {
    let mut sets: Vec<(usize, Fill)> = Vec::new();
    for a in assignments {
        let c = match &a.target {
            AssignmentTarget::ColumnName(n) => column_index(table, n)?,
            AssignmentTarget::Tuple(_) => return Err(refused("an UPDATE of a tuple")),
        };
        if sets.iter().any(|(s, _)| *s == c) {
            return Err(Error::new(format!(
                "multiple assignments to same column \"{}\"",
                table.columns[c].name
            )));
        }
        if is_default(&a.value) {
            sets.push((c, Fill::Default));
        } else if is_literal(&a.value) || crate::composed::constructs(table.columns[c].ty, &a.value)
        {
            sets.push((c, Fill::Literal(datum(&a.value, table, c)?)));
        } else {
            sets.push((c, Fill::Expression(a.value.to_string())));
        }
    }
    Ok(sets)
}

/// The tokens of a statement, each with where it begins and ends in the text, in bytes.
pub(crate) fn spans(sql: &str) -> Result<Vec<(sqlparser::tokenizer::Token, usize, usize)>> {
    let tokens = sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, sql)
        .tokenize_with_location()?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let at = |l: sqlparser::tokenizer::Location| -> usize {
        let line = starts
            .get(l.line.saturating_sub(1) as usize)
            .copied()
            .unwrap_or(sql.len());
        sql[line..]
            .char_indices()
            .nth(l.column.saturating_sub(1) as usize)
            .map_or(sql.len(), |(i, _)| line + i)
    };
    Ok(tokens
        .into_iter()
        .map(|t| {
            let (a, b) = (at(t.span.start), at(t.span.end));
            (t.token, a, b)
        })
        .collect())
}

/// A statement with each parameter, `$1` and on, written where it stands as a quoted literal of
/// its text: unknown-typed, as PostgreSQL types a parameter a driver sends with no type, and read
/// by its context.
pub(crate) fn with_params(sql: &str, params: &[String]) -> Result<String> {
    let mut out = String::with_capacity(sql.len());
    let mut from = 0;
    for (t, a, b) in spans(sql)? {
        let sqlparser::tokenizer::Token::Placeholder(p) = t else {
            continue;
        };
        let Some(n) = p.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) else {
            continue;
        };
        let given = params
            .get(n.wrapping_sub(1))
            .ok_or_else(|| Error::new(format!("there is no parameter ${n}")))?;
        out.push_str(&sql[from..a]);
        out.push('\'');
        out.push_str(&given.replace('\'', "''"));
        out.push('\'');
        from = b;
    }
    out.push_str(&sql[from..]);
    Ok(out)
}

/// The statements of a text, each as written, split on their semicolons by sqlparser's tokenizer.
pub fn split(text: &str) -> Result<Vec<String>> {
    Ok(crate::ddl::statements(text)?
        .iter()
        .map(|t| crate::ddl::written(t))
        .collect())
}

pub fn parse(catalog: &Catalog, sql: &str) -> Result<Statement> {
    let sql = &*super::spelled::scanned(sql, catalog.release)?;
    // A declaration goes to the catalog as written, where the snail reads what sqlparser does not.
    if let Some(kind) = crate::ddl::declares(sql) {
        let ddl = sql.trim().trim_end_matches(';').trim_end().to_string();
        if crate::ddl::statements(&ddl)?.len() != 1 {
            return Err(Error::new("one statement at a time"));
        }
        return Ok(Statement::Declare { ddl, kind });
    }
    let tokens = sqlparser::tokenizer::Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize()?;
    if let Some(st) = transaction_statement(&tokens)? {
        return Ok(st);
    }
    if let Some(read) = crate::ddl::set_constraints(&tokens) {
        let (names, deferred) = read?;
        return Ok(Statement::Constraints { names, deferred });
    }
    if let Some(said) = super::session::said(catalog, sql, &tokens)? {
        return Ok(Statement::Session(said));
    }
    if tokens.iter().any(|t| matches!(t, sqlparser::tokenizer::Token::Word(w) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("overriding"))) {
        return Err(refused("OVERRIDING SYSTEM VALUE or USER VALUE"));
    }
    // A question with locking clauses is asked as it stands: sqlparser reads only two of them.
    let first = tokens
        .iter()
        .find(|t| !matches!(t, sqlparser::tokenizer::Token::Whitespace(_)));
    let select = matches!(first, Some(sqlparser::tokenizer::Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("select"));
    if select && super::locking::clauses(sql)?.is_some() {
        return Ok(Statement::Question {
            question: sql.trim().trim_end_matches(';').trim_end().to_string(),
        });
    }
    super::question::unlocked_words(sql)?;
    super::spelled::at_at_spaced(sql)?;
    let bound = crate::grammar::bound(sql);
    let statements = super::image::parse_sql(bound.as_deref().unwrap_or(sql))?;
    let s = match statements.as_slice() {
        [s] => s,
        _ => return Err(Error::new("one statement at a time")),
    };
    // A write to a view is the write to the table under it, its views' CHECK OPTIONs held to.
    if super::updatable::writes_a_view(catalog, s) {
        let (st, checks) = super::updatable::rewritten(catalog, s)?;
        let checks: Vec<(String, String)> =
            checks.into_iter().map(|(v, e)| (v, e.to_string())).collect();
        let checked = |write: Statement| match checks.is_empty() {
            true => write,
            false => Statement::Checked {
                write: Box::new(write),
                checks: checks.clone(),
            },
        };
        return Ok(match parse(catalog, &st.to_string())? {
            Statement::Returning { write, alias, items } => Statement::Returning {
                write: Box::new(checked(*write)),
                alias,
                items,
            },
            write => checked(write),
        });
    }
    match s {
        Sql::Commit { chain: false, .. } => Ok(Statement::Commit),
        Sql::Rollback {
            chain: false,
            savepoint: None,
        } => Ok(Statement::Rollback),
        Sql::Commit { chain: true, .. } => Ok(Statement::Session(super::session::Said::Chain {
            commit: true,
        })),
        Sql::Rollback {
            chain: true,
            savepoint: None,
        } => Ok(Statement::Session(super::session::Said::Chain {
            commit: false,
        })),
        Sql::Rollback {
            chain: true,
            savepoint: Some(_),
        } => Err(Error::new("syntax error at or near \"TO\"")),
        Sql::Rollback {
            savepoint: Some(name),
            ..
        } => Ok(Statement::Session(super::session::Said::RollbackTo {
            name: ident(name),
        })),
        Sql::Savepoint { name } => Ok(Statement::Session(super::session::Said::Savepoint {
            name: ident(name),
        })),
        Sql::ReleaseSavepoint { name } => Ok(Statement::Session(super::session::Said::Release {
            name: ident(name),
        })),
        Sql::Insert(i) => {
            let alias = match (&i.table_alias, &i.table) {
                (Some(a), _) => ident(&a.alias),
                (None, TableObject::TableName(n)) => {
                    let n = object_name(n);
                    n.rsplit('.').next().unwrap_or(&n).to_string()
                }
                _ => String::new(),
            };
            Ok(returning(insert(catalog, i)?, alias, &i.returning))
        }
        Sql::Update(u) => {
            if !u.order_by.is_empty() || u.limit.is_some() {
                return Err(refused("an UPDATE with ORDER BY or LIMIT"));
            }
            let ti = one_table(catalog, &u.table)?;
            let table = &catalog.tables[ti];
            let mut sets = sets_of(table, &u.assignments)?;
            let find = match &u.from {
                None => finder(table, &u.table, u.selection.as_ref()),
                // Joined with its FROM: each expression set read on the joined row, an answer of
                // the question that finds the rows.
                Some(from) => {
                    let (sqlparser::ast::UpdateTableFromKind::BeforeSet(from)
                    | sqlparser::ast::UpdateTableFromKind::AfterSet(from)) = from;
                    let mut asked = Vec::new();
                    for (_, f) in sets.iter_mut() {
                        if let Fill::Expression(e) = f {
                            asked.push(format!("({e})"));
                            *f = Fill::Answer(asked.len() - 1);
                        }
                    }
                    joined_finder(table, &u.table, from, &asked, u.selection.as_ref())
                }
            };
            let set = Statement::Set {
                table: ti,
                find,
                sets,
            };
            Ok(returning(set, read_as(&u.table), &u.returning))
        }
        Sql::Delete(d) => {
            if !d.tables.is_empty() || !d.order_by.is_empty() || d.limit.is_some() {
                return Err(refused(
                    "a DELETE of several tables, or with ORDER BY or LIMIT",
                ));
            }
            let from = match &d.from {
                FromTable::WithFromKeyword(f) | FromTable::WithoutKeyword(f) => f,
            };
            let t = match from.as_slice() {
                [t] => t,
                _ => return Err(Error::new("a DELETE names one table")),
            };
            let ti = one_table(catalog, t)?;
            let find = match &d.using {
                None => finder(&catalog.tables[ti], t, d.selection.as_ref()),
                Some(using) => {
                    joined_finder(&catalog.tables[ti], t, using, &[], d.selection.as_ref())
                }
            };
            let delete = Statement::Delete { table: ti, find };
            Ok(returning(delete, read_as(t), &d.returning))
        }
        // A WITH holding writes: each read as a statement of its own.
        Sql::Query(q)
            if q
                .with
                .as_ref()
                .is_some_and(|w| w.cte_tables.iter().any(|c| writes_query(&c.query))) =>
        {
            let mut writes = Vec::new();
            for c in &q.with.as_ref().unwrap().cte_tables {
                if !writes_query(&c.query) {
                    continue;
                }
                let written = match c.query.body.as_ref() {
                    SetExpr::Insert(s) | SetExpr::Update(s) | SetExpr::Delete(s)
                        if c.query.with.is_none() =>
                    {
                        s.to_string()
                    }
                    SetExpr::Merge(_) => return Err(refused("MERGE in a WITH")),
                    _ => return Err(refused("a WITH of its own on a write inside WITH")),
                };
                writes.push((ident(&c.alias.name), parse(catalog, &written)?));
            }
            Ok(Statement::Writes {
                writes,
                question: sql.trim().trim_end_matches(';').trim_end().to_string(),
            })
        }
        Sql::Query(_) => Ok(Statement::Question {
            question: sql.trim().trim_end_matches(';').trim_end().to_string(),
        }),
        Sql::Truncate { .. } => Err(refused("TRUNCATE")),
        Sql::Set(sqlparser::ast::Set::SingleAssignment {
            variable, values, ..
        }) => setting(&object_name(variable), values, catalog),
        Sql::Set(sqlparser::ast::Set::SetTimeZone { value, .. }) => {
            setting("timezone", std::slice::from_ref(value), catalog)
        }
        other => Err(Error::new(format!(
            "{other} is not a statement the snail writes"
        ))),
    }
}

/// The settings the snail answers under, each its name as PostgreSQL's SET takes it and its
/// value: what PostgreSQL's side of a certificate runs under too. lc_monetary is the one the file's
/// money was written under, C where it names no other; default_text_search_config the file's,
/// pg_catalog.simple where it names no other.
pub const SETTINGS: [(&str, &str); 10] = [
    ("timezone", "UTC"),
    ("datestyle", "ISO, MDY"),
    ("intervalstyle", "postgres"),
    ("extra_float_digits", "1"),
    ("bytea_output", "hex"),
    ("standard_conforming_strings", "on"),
    ("lc_monetary", "C"),
    ("lc_numeric", "C"),
    ("lc_time", "C"),
    ("default_text_search_config", crate::file::TEXT_SEARCH),
];

/// A SET of `name` to `values`: taken, changing nothing, where it sets the value the snail answers
/// under, the file's lc_monetary and default_text_search_config among them, and refused by name
/// where it sets another or a setting the snail does not read.
fn setting(name: &str, values: &[Expr], catalog: &Catalog) -> Result<Statement> {
    let name = name.to_lowercase();
    // Each value as SET reads it: a word, a quoted text or a number.
    let words: Vec<String> = values
        .iter()
        .map(|v| match v {
            Expr::Value(v) => match &v.value {
                Value::SingleQuotedString(s) | Value::Number(s, _) => s.clone(),
                Value::Boolean(b) => b.to_string(),
                other => other.to_string(),
            },
            Expr::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect();
    fixed_setting(&name, &words, catalog.monetary, catalog.text_search)?;
    Ok(Statement::Setting { name })
}

/// A SET of a fixed setting `name` to `words`: taken where it sets the value the snail answers
/// under, `monetary` lc_monetary's and `text_search` default_text_search_config's, and refused by
/// name where it sets another or a setting the snail does not fix.
pub(super) fn fixed_setting(
    name: &str,
    words: &[String],
    monetary: crate::monetary::Monetary,
    text_search: crate::tsearch::TextSearch,
) -> Result<()> {
    let name = name.to_lowercase();
    let file = monetary.name();
    let Some(&(_, fixed)) = SETTINGS.iter().find(|(n, _)| *n == name) else {
        return Err(Error::new(format!(
            "SET {name} is not read: the snail answers under fixed settings"
        )));
    };
    let joined = words.join(", ");
    let same = match name.as_str() {
        "datestyle" => {
            // Each part of a DateStyle sets its output or its order of fields, as PostgreSQL's
            // check_datestyle reads it; the parts it does not name keep the fixed ones.
            let (mut style, mut order) = ("iso", "mdy");
            let mut known = true;
            for part in joined.split(',').map(|p| p.trim().to_lowercase()) {
                match part.as_str() {
                    "iso" | "sql" | "postgres" | "german" => {
                        style = if part == "iso" { "iso" } else { "other" }
                    }
                    "ymd" | "dmy" | "euro" | "european" => order = "other",
                    "mdy" | "us" | "noneuro" | "noneuropean" | "default" => {}
                    _ => known = false,
                }
            }
            known && style == "iso" && order == "mdy"
        }
        "standard_conforming_strings" => {
            matches!(
                joined.to_lowercase().as_str(),
                "on" | "true" | "yes" | "1" | "default"
            )
        }
        "lc_monetary" => {
            crate::monetary::Monetary::names_same(&joined, &file)
                || joined.eq_ignore_ascii_case("default")
        }
        "lc_numeric" | "lc_time" => {
            matches!(joined.as_str(), "C" | "POSIX") || joined.eq_ignore_ascii_case("default")
        }
        "default_text_search_config" => {
            crate::tsearch::TextSearch::open(&joined).is_ok_and(|t| t == text_search)
                || joined.eq_ignore_ascii_case("default")
        }
        _ => {
            joined.eq_ignore_ascii_case(fixed)
                || joined.eq_ignore_ascii_case("default")
                // A number or a word of a list as PostgreSQL's SET reads it: ` +1 ` is 1.
                || super::settings::find(crate::regs::LATEST, &name).is_some_and(|g| {
                    matches!(
                        (
                            super::settings::normalized(g, &joined),
                            super::settings::normalized(g, fixed)
                        ),
                        (Ok(a), Ok(b)) if a == b
                    )
                })
        }
    };
    let fixed = match name.as_str() {
        "lc_monetary" => file.as_str(),
        "default_text_search_config" => text_search.name(),
        _ => fixed,
    };
    if !same {
        return Err(Error::new(format!(
            "SET {name} to {joined} is refused: the snail answers under {name} {fixed}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> Catalog {
        Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b text, at timestamptz, PRIMARY KEY (a));
             CREATE TABLE s.u (a integer NOT NULL, n smallint, PRIMARY KEY (a));",
        )
        .unwrap()
    }

    #[test]
    fn each_write_is_read_with_its_rows_or_the_question_that_finds_them() {
        let c = catalog();
        match parse(&c, "INSERT INTO s.t (b, a) VALUES ('x', 1), (NULL, -2)").unwrap() {
            Statement::Insert {
                table: 0,
                source: Source::Filled(rows),
            } => {
                // A column the INSERT leaves out, or gives DEFAULT, takes its DEFAULT.
                let shown: Vec<String> = rows.iter().map(|r| format!("{r:?}")).collect();
                assert_eq!(
                    shown,
                    [
                        "[Literal(Int(1)), Literal(Text(\"x\")), Default]",
                        "[Literal(Int(-2)), Literal(Null), Default]",
                    ]
                );
            }
            other => panic!("{other:?}"),
        }
        match parse(
            &c,
            "INSERT INTO s.t (a, b, at) SELECT u.a, 'y', '2020-01-02 03:04:05+00' FROM s.u u WHERE u.n = 3",
        )
        .unwrap()
        {
            Statement::Insert {
                source: Source::Select { question, fill },
                ..
            } => {
                assert_eq!(question, "SELECT u.a FROM s.u u WHERE u.n = 3");
                assert!(matches!(fill[0], Fill::Answer(0)));
                assert!(matches!(&fill[1], Fill::Literal(Datum::Text(s)) if s == "y"));
                assert!(matches!(fill[2], Fill::Literal(Datum::Int(1_577_934_245_000_000))));
            }
            other => panic!("{other:?}"),
        }
        match parse(&c, "UPDATE s.t SET b = 'z' WHERE a = 1").unwrap() {
            Statement::Set { table, find, sets } => {
                assert_eq!(table, 0);
                assert_eq!(find, "SELECT \"a\", \"b\", \"at\" FROM s.t WHERE a = 1");
                assert!(matches!(&sets[..], [(1, Fill::Literal(Datum::Text(z)))] if z == "z"));
            }
            other => panic!("{other:?}"),
        }
        // A value an expression sets is read on the row's old values the finder finds.
        match parse(&c, "UPDATE s.t SET b = b || 'z', a = a + $1 WHERE a = 1").unwrap() {
            Statement::Set { find, sets, .. } => {
                assert_eq!(find, "SELECT \"a\", \"b\", \"at\" FROM s.t WHERE a = 1");
                assert!(matches!(
                    &sets[..],
                    [(1, Fill::Expression(b)), (0, Fill::Expression(a))] if b == "b || 'z'" && a == "a + $1"
                ));
            }
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            parse(&c, "DELETE FROM s.u").unwrap(),
            Statement::Delete { table: 1, .. }
        ));
        for (sql, kind) in [
            ("BEGIN", "BEGIN"),
            ("COMMIT", "COMMIT"),
            ("ROLLBACK", "ROLLBACK"),
        ] {
            assert_eq!(parse(&c, sql).unwrap().kind(), kind);
        }
    }

    #[test]
    fn what_is_not_written_is_refused_with_its_reason() {
        let c = catalog();
        for (sql, why) in [
            (
                "INSERT INTO s.t (a) OVERRIDING SYSTEM VALUE VALUES (1)",
                "OVERRIDING",
            ),
            (
                "UPDATE s.t SET b = 'x', b = 'y' WHERE a = 1",
                "multiple assignments",
            ),
            ("INSERT INTO s.u (n, a) VALUES (70000, 1)", "out of range"),
            ("TRUNCATE s.t", "TRUNCATE"),
            ("BEGIN; COMMIT", "one statement at a time"),
        ] {
            let e = parse(&c, sql).unwrap_err().to_string();
            assert!(e.contains(why), "{sql}: {e}");
        }
    }
}
