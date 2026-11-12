//! The DDL the snail reads itself, token by token, where sqlparser 0.63 reads it otherwise than it is
//! written, or not at all: CREATE TYPE and ALTER TYPE, CREATE DOMAIN and ALTER DOMAIN, CREATE
//! SEQUENCE and ALTER SEQUENCE, CREATE COLLATION; and, inside a CREATE TABLE, a column's identity, its
//! generation expression and its UNIQUE NULLS [NOT] DISTINCT, taken out before sqlparser reads the
//! rest. Each form refuses by name what it does not know.

use crate::catalog::{
    assignable, format_type, held_type, BodyForm, ident, level, object_name, shape, Action, Brin, Catalog,
    Check, Collation, Column, ColumnType, Domain, DomainCheck, Fact, ForeignKey, Function,
    Generated, Identity, KeyConstraint, Level, Method, Order, Reading, Sequence, SequenceType,
    Shape, StatementRead, Table, Type, TypeKind, View, TEXT_OPCLASSES,
};
use crate::{Error, Result};
use sqlparser::ast::helpers::stmt_create_table::CreateTableBuilder;
use std::collections::HashMap;
use sqlparser::ast::{
    ColumnOption, ConstraintCharacteristics, CreateFunctionBody, DataType, DeferrableInitial, Expr,
    FunctionArg, FunctionArgExpr, FunctionArguments, NullsDistinctOption, ObjectNamePart,
    OrderBySort, SelectItem, SetExpr, Statement, TableConstraint, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer, Whitespace};

/// A statement's tokens without their white space and comments, read in turn.
pub(crate) struct Cursor {
    t: Vec<Token>,
    i: usize,
}

/// An identifier as PostgreSQL keeps it: folded to lower case unless quoted.
fn folded(w: &sqlparser::tokenizer::Word) -> String {
    if w.quote_style.is_some() {
        w.value.clone()
    } else {
        w.value.to_lowercase()
    }
}

impl Cursor {
    pub(crate) fn new(tokens: &[Token]) -> Cursor {
        Cursor {
            t: tokens
                .iter()
                .filter(|t| !matches!(t, Token::Whitespace(_)))
                .cloned()
                .collect(),
            i: 0,
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.t.get(self.i)
    }

    fn peek_at(&self, k: usize) -> Option<&Token> {
        self.t.get(self.i + k)
    }

    pub(crate) fn done(&self) -> bool {
        self.i >= self.t.len()
    }

    fn word_is(t: Option<&Token>, k: &str) -> bool {
        matches!(t, Some(Token::Word(w)) if w.quote_style.is_none() && w.value.eq_ignore_ascii_case(k))
    }

    pub(crate) fn is(&self, k: &str) -> bool {
        Self::word_is(self.peek(), k)
    }

    /// Takes the keyword where it comes next.
    pub(crate) fn kw(&mut self, k: &str) -> bool {
        if self.is(k) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Takes the keywords where all of them come next, in turn.
    pub(crate) fn kws(&mut self, ks: &[&str]) -> bool {
        if ks
            .iter()
            .enumerate()
            .all(|(n, k)| Self::word_is(self.peek_at(n), k))
        {
            self.i += ks.len();
            true
        } else {
            false
        }
    }

    pub(crate) fn expect(&mut self, k: &str) -> Result<()> {
        if self.kw(k) {
            Ok(())
        } else {
            Err(self.wanted(k))
        }
    }

    fn wanted(&self, what: &str) -> Error {
        match self.peek() {
            Some(t) => Error::new(format!("{what} is wanted where {t} is written")),
            None => Error::new(format!("{what} is wanted where the statement ends")),
        }
    }

    fn token(&mut self, t: Token) -> bool {
        if self.peek() == Some(&t) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect_token(&mut self, t: Token) -> Result<()> {
        if self.token(t.clone()) {
            Ok(())
        } else {
            Err(self.wanted(&t.to_string()))
        }
    }

    /// One identifier, folded unless quoted.
    pub(crate) fn ident(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::Word(w)) => {
                let s = folded(w);
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.wanted("a name")),
        }
    }

    /// A name, qualified or not, its parts folded unless quoted and joined by a period.
    pub(crate) fn name(&mut self) -> Result<String> {
        let mut parts = vec![self.ident()?];
        while self.peek() == Some(&Token::Period) {
            self.i += 1;
            parts.push(self.ident()?);
        }
        Ok(parts.join("."))
    }

    pub(crate) fn string(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::SingleQuotedString(s)) => {
                let s = s.clone();
                self.i += 1;
                Ok(s)
            }
            _ => Err(self.wanted("a quoted string")),
        }
    }

    /// A whole number, signed or not.
    pub(crate) fn integer(&mut self) -> Result<i64> {
        let neg = if self.token(Token::Minus) {
            true
        } else {
            self.token(Token::Plus);
            false
        };
        match self.peek() {
            Some(Token::Number(n, _)) => {
                let text = if neg { format!("-{n}") } else { n.clone() };
                let v = text
                    .parse::<i64>()
                    .map_err(|_| Error::new(format!("{text} is not a bigint")))?;
                self.i += 1;
                Ok(v)
            }
            _ => Err(self.wanted("a number")),
        }
    }

    /// A value an option is set to: a name, a quoted string or a number, as its text.
    fn option_value(&mut self) -> Result<String> {
        match self.peek() {
            Some(Token::SingleQuotedString(_)) => self.string(),
            Some(Token::Number(..)) | Some(Token::Minus) | Some(Token::Plus) => {
                Ok(self.integer()?.to_string())
            }
            _ => self.name(),
        }
    }

    /// The tokens inside the parentheses that come next, the parentheses taken.
    pub(crate) fn group(&mut self) -> Result<Vec<Token>> {
        self.expect_token(Token::LParen)?;
        let start = self.i;
        let mut depth = 1;
        while let Some(t) = self.peek() {
            match t {
                Token::LParen => depth += 1,
                Token::RParen => {
                    depth -= 1;
                    if depth == 0 {
                        let inner = self.t[start..self.i].to_vec();
                        self.i += 1;
                        return Ok(inner);
                    }
                }
                _ => {}
            }
            self.i += 1;
        }
        Err(Error::new("a parenthesis is left open"))
    }

    /// The tokens up to the first comma outside parentheses, or the end; the comma taken.
    fn item(&mut self) -> Vec<Token> {
        let start = self.i;
        let mut depth = 0;
        while let Some(t) = self.peek() {
            match t {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                Token::Comma if depth == 0 => {
                    let out = self.t[start..self.i].to_vec();
                    self.i += 1;
                    return out;
                }
                _ => {}
            }
            self.i += 1;
        }
        self.t[start..self.i].to_vec()
    }

    /// The comma-separated items of the parentheses that come next.
    fn items(&mut self) -> Result<Vec<Vec<Token>>> {
        let inner = self.group()?;
        let mut c = Cursor { t: inner, i: 0 };
        let mut out = Vec::new();
        while !c.done() {
            out.push(c.item());
        }
        Ok(out)
    }

    /// An expression, read by sqlparser up to the first of `stops` standing outside parentheses
    /// after its first token, or the end; all of it must be read.
    pub(crate) fn expr_until(&mut self, stops: &[&str]) -> Result<Expr> {
        let start = self.i;
        let mut depth = 0;
        let mut end = self.t.len();
        for (k, t) in self.t.iter().enumerate().skip(start) {
            match t {
                Token::LParen | Token::LBracket => depth += 1,
                Token::RParen | Token::RBracket => depth -= 1,
                _ if depth == 0 && k > start && stops.iter().any(|s| Self::word_is(Some(t), s)) => {
                    end = k;
                    break;
                }
                _ => {}
            }
        }
        let e = expression(&self.t[start..end])?;
        self.i = end;
        Ok(e)
    }

    /// A data type, read by sqlparser from the tokens up to the first comma or the end.
    fn data_type_item(tokens: &[Token]) -> Result<DataType> {
        let mut p = Parser::new(&PostgreSqlDialect {}).with_tokens(tokens.to_vec());
        let dt = p.parse_data_type()?;
        if p.index() < tokens.len() {
            return Err(Error::new(format!(
                "{} is read after the type {dt}",
                text(&tokens[p.index()..])
            )));
        }
        Ok(dt)
    }

    /// A data type, read by sqlparser from where the cursor stands.
    fn data_type(&mut self) -> Result<DataType> {
        let mut p = Parser::new(&PostgreSqlDialect {}).with_tokens(self.t[self.i..].to_vec());
        let dt = p.parse_data_type()?;
        self.i += p.index();
        Ok(dt)
    }

    pub(crate) fn rest(&self) -> String {
        text(&self.t[self.i.min(self.t.len())..])
    }
}

/// Tokens as text, a space between each.
pub(crate) fn text(tokens: &[Token]) -> String {
    tokens
        .iter()
        .map(|t| t.to_string())
        .collect::<Vec<_>>()
        .join(" ")
}

/// An expression read by sqlparser from tokens, every one of them read.
pub(crate) fn expression(tokens: &[Token]) -> Result<Expr> {
    if tokens.is_empty() {
        return Err(Error::new("an expression is wanted"));
    }
    let mut p = Parser::new(&crate::grammar::POSTGRES).with_tokens(tokens.to_vec());
    let e = p.parse_expr()?;
    if p.index() < tokens.len() {
        return Err(Error::new(format!(
            "{} is read after the expression {e}",
            text(&tokens[p.index()..])
        )));
    }
    Ok(crate::grammar::expression_of(tokens).unwrap_or(e))
}

/// A statement the snail reads itself.
pub(crate) enum Form {
    /// COMMENT ON: the object described, and its text, none for IS NULL.
    Comment {
        target: Commented,
        text: Option<String>,
    },
    CreateType(Type),
    AlterType(String, AlterType),
    CreateDomain(Domain),
    AlterDomain(String, AlterDomain),
    CreateSequence {
        sequence: Sequence,
        if_not_exists: bool,
        options: Vec<SequenceOption>,
    },
    AlterSequence {
        name: String,
        if_exists: bool,
        options: Vec<SequenceOption>,
    },
    CreateCollation {
        collation: Collation,
        if_not_exists: bool,
    },
    CreateSchema {
        name: String,
        if_not_exists: bool,
    },
    DropSchema {
        names: Vec<String>,
        if_exists: bool,
        cascade: bool,
    },
    DropRelations {
        kind: Dropped,
        /// Each relation named: its schema where the name gives one, and its own name.
        names: Vec<(Option<String>, String)>,
        if_exists: bool,
        cascade: bool,
    },
    /// DROP TYPE, or DROP DOMAIN where `domain`.
    DropTypes {
        domain: bool,
        names: Vec<TypeNamed>,
        if_exists: bool,
        cascade: bool,
    },
    DropFunctions {
        names: Vec<FunctionNamed>,
        if_exists: bool,
        cascade: bool,
    },
}

/// A type a DROP TYPE or DROP DOMAIN names: its words, as a type's name is read; its schema where
/// the name gives one; and the name as PostgreSQL shows it, its parts unquoted and `[]` where it
/// names an array.
#[derive(Clone, Debug)]
pub(crate) struct TypeNamed {
    words: String,
    schema: Option<String>,
    shown: String,
}

/// The kind of relation a DROP names: DROP TABLE, VIEW, SEQUENCE or INDEX.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Dropped {
    Table,
    View,
    Sequence,
    Index,
}

impl Dropped {
    /// The kind as pg_class's relkind gives it.
    fn relkind(self) -> char {
        match self {
            Dropped::Table => 'r',
            Dropped::View => 'v',
            Dropped::Sequence => 'S',
            Dropped::Index => 'i',
        }
    }

    /// The word PostgreSQL's DROP names the kind by.
    fn word(self) -> &'static str {
        match self {
            Dropped::Table => "table",
            Dropped::View => "view",
            Dropped::Sequence => "sequence",
            Dropped::Index => "index",
        }
    }

    fn one(self) -> &'static str {
        match self {
            Dropped::Index => "an index",
            Dropped::Table => "a table",
            Dropped::View => "a view",
            Dropped::Sequence => "a sequence",
        }
    }
}

pub(crate) enum AlterType {
    AddValue {
        label: String,
        if_not_exists: bool,
        /// The label it goes before (true) or after (false).
        beside: Option<(bool, String)>,
    },
    RenameValue {
        from: String,
        to: String,
    },
}

pub(crate) enum AlterDomain {
    SetDefault(Expr),
    DropDefault,
    SetNotNull,
    DropNotNull,
    AddCheck {
        name: Option<String>,
        check: Expr,
        valid: bool,
    },
    AddNotNull {
        name: Option<String>,
    },
    DropConstraint {
        name: String,
        if_exists: bool,
    },
    RenameConstraint {
        from: String,
        to: String,
    },
    ValidateConstraint(String),
}

/// One option of a sequence, as CREATE SEQUENCE, ALTER SEQUENCE and an identity column write it.
#[derive(Clone, Debug)]
pub(crate) enum SequenceOption {
    As(SequenceType),
    Increment(i64),
    MinValue(Option<i64>),
    MaxValue(Option<i64>),
    Start(i64),
    Restart(Option<i64>),
    Cache(i64),
    Cycle(bool),
    OwnedBy(Option<(String, String)>),
    /// An identity column's sequence named.
    SequenceName(String),
}

/// Reads a statement the snail reads itself; none where it is no such form.
pub(crate) fn read(tokens: &[Token]) -> Option<Result<Form>> {
    let mut c = Cursor::new(tokens);
    if c.kws(&["CREATE", "TYPE"]) {
        return Some(create_type(&mut c));
    }
    if c.kws(&["ALTER", "TYPE"]) {
        return Some(alter_type(&mut c));
    }
    if c.kws(&["CREATE", "DOMAIN"]) {
        return Some(create_domain(&mut c));
    }
    if c.kws(&["ALTER", "DOMAIN"]) {
        return Some(alter_domain(&mut c));
    }
    for kind in ["TEMP", "TEMPORARY", "UNLOGGED"] {
        if c.kws(&["CREATE", kind, "SEQUENCE"]) {
            return Some(Err(Error::new(format!(
                "CREATE {kind} SEQUENCE: a snail's sequences are kept in its file, so none is {}",
                if kind == "UNLOGGED" {
                    "unlogged"
                } else {
                    "temporary"
                }
            ))));
        }
    }
    if c.kws(&["CREATE", "SEQUENCE"]) {
        return Some(create_sequence(&mut c));
    }
    if c.kws(&["ALTER", "SEQUENCE"]) {
        return Some(alter_sequence(&mut c));
    }
    if c.kws(&["CREATE", "COLLATION"]) {
        return Some(create_collation(&mut c));
    }
    if c.kws(&["CREATE", "SCHEMA"]) {
        return Some(create_schema(&mut c));
    }
    if c.kws(&["DROP", "SCHEMA"]) {
        return Some(drop_schema(&mut c));
    }
    for (word, domain) in [("TYPE", false), ("DOMAIN", true)] {
        if c.kws(&["DROP", word]) {
            return Some(drop_types(&mut c, domain));
        }
    }
    if c.kws(&["DROP", "FUNCTION"]) {
        return Some(drop_functions(&mut c));
    }
    if c.kws(&["COMMENT", "ON"]) {
        return Some(comment_on(&mut c));
    }
    for (word, kind) in [
        ("TABLE", Dropped::Table),
        ("VIEW", Dropped::View),
        ("SEQUENCE", Dropped::Sequence),
        ("INDEX", Dropped::Index),
    ] {
        if c.kws(&["DROP", word]) {
            return Some(drop_relations(&mut c, kind));
        }
    }
    None
}

/// DROP { TABLE | VIEW | SEQUENCE | INDEX [CONCURRENTLY] } [IF EXISTS] name [, …] [CASCADE |
/// RESTRICT]. CONCURRENTLY is read, and drops the index as DROP INDEX does: a snail lays an index
/// out in one round, and takes it out in one.
fn drop_relations(c: &mut Cursor, kind: Dropped) -> Result<Form> {
    let concurrently = kind == Dropped::Index && c.kw("CONCURRENTLY");
    let if_exists = c.kws(&["IF", "EXISTS"]);
    let mut names = Vec::new();
    loop {
        let first = c.ident()?;
        let name = match c.token(Token::Period) {
            true => (Some(first), c.ident()?),
            false => (None, first),
        };
        if c.peek() == Some(&Token::Period) {
            return Err(Error::new(
                "DROP TABLE names a table by its database: cross-database references are not \
                 implemented",
            ));
        }
        names.push(name);
        if !c.token(Token::Comma) {
            break;
        }
    }
    let cascade = c.kw("CASCADE");
    if !cascade {
        c.kw("RESTRICT");
    }
    end(c, &format!("DROP {}", kind.word().to_uppercase()))?;
    if concurrently && names.len() > 1 {
        return Err(Error::new(
            "DROP INDEX CONCURRENTLY does not support dropping multiple objects",
        ));
    }
    if concurrently && cascade {
        return Err(Error::new("DROP INDEX CONCURRENTLY does not support CASCADE"));
    }
    Ok(Form::DropRelations {
        kind,
        names,
        if_exists,
        cascade,
    })
}

/// CREATE SCHEMA [IF NOT EXISTS] name. AUTHORIZATION names a role, and the statements a schema is
/// made with read their names by a search path that begins with it: each refused by name.
fn create_schema(c: &mut Cursor) -> Result<Form> {
    let if_not_exists = c.kws(&["IF", "NOT", "EXISTS"]);
    let authorization = || {
        Error::new(
            "CREATE SCHEMA … AUTHORIZATION is refused: roles and privileges are the server's own, \
             and a snail has none",
        )
    };
    if c.is("AUTHORIZATION") {
        return Err(authorization());
    }
    let name = c.ident()?;
    if c.is("AUTHORIZATION") {
        return Err(authorization());
    }
    if c.is("CREATE") || c.is("GRANT") {
        return Err(Error::new(format!(
            "CREATE SCHEMA {name} {} is refused: the statements a schema is made with read their \
             names by a search path that begins with it, and a snail names each table by its \
             schema, and has no search path",
            c.rest()
        )));
    }
    end(c, "CREATE SCHEMA")?;
    Ok(Form::CreateSchema {
        name,
        if_not_exists,
    })
}

/// DROP SCHEMA [IF EXISTS] name [, …] [CASCADE | RESTRICT].
fn drop_schema(c: &mut Cursor) -> Result<Form> {
    let if_exists = c.kws(&["IF", "EXISTS"]);
    let mut names = vec![c.ident()?];
    while c.token(Token::Comma) {
        names.push(c.ident()?);
    }
    let cascade = c.kw("CASCADE");
    if !cascade {
        c.kw("RESTRICT");
    }
    end(c, "DROP SCHEMA")?;
    Ok(Form::DropSchema {
        names,
        if_exists,
        cascade,
    })
}

/// DROP { TYPE | DOMAIN } [IF EXISTS] name [, …] [CASCADE | RESTRICT]: each name a type's, as
/// PostgreSQL's grammar takes a type name, array bounds and all.
fn drop_types(c: &mut Cursor, domain: bool) -> Result<Form> {
    let if_exists = c.kws(&["IF", "EXISTS"]);
    let (items, cascade) = dropped_list(c)?;
    Ok(Form::DropTypes {
        domain,
        names: items.iter().map(|item| type_named(item)).collect(),
        if_exists,
        cascade,
    })
}

/// The objects a DROP names, each its tokens up to a comma outside parentheses, and whether CASCADE
/// closes the list; RESTRICT read and taken. The rest of the statement is taken.
fn dropped_list(c: &mut Cursor) -> Result<(Vec<Vec<Token>>, bool)> {
    let last = c.t.len().saturating_sub(1);
    let cascade = c.t.len() > c.i && Cursor::word_is(c.t.get(last), "CASCADE");
    let end = match cascade || (c.t.len() > c.i && Cursor::word_is(c.t.get(last), "RESTRICT")) {
        true => last,
        false => c.t.len(),
    };
    let mut list = Cursor {
        t: c.t[c.i..end].to_vec(),
        i: 0,
    };
    c.i = c.t.len();
    let mut items = Vec::new();
    loop {
        let item = list.item();
        if item.is_empty() {
            return Err(match list.peek() {
                Some(t) => Error::new(format!("syntax error at or near \"{t}\"")),
                None => Error::new("syntax error at end of input"),
            });
        }
        items.push(item);
        if list.done() && list.t.last() != Some(&Token::Comma) {
            break;
        }
    }
    Ok((items, cascade))
}

/// A function a DROP FUNCTION names: its name, and its arguments' types where it lists them, an OUT
/// argument's left out as PostgreSQL leaves it out of a function's identity.
#[derive(Clone, Debug)]
pub(crate) struct FunctionNamed {
    name: String,
    args: Option<Vec<DataType>>,
}

/// DROP FUNCTION [IF EXISTS] name [( [[mode] [name] type [, …]] )] [, …] [CASCADE | RESTRICT].
fn drop_functions(c: &mut Cursor) -> Result<Form> {
    let if_exists = c.kws(&["IF", "EXISTS"]);
    let (items, cascade) = dropped_list(c)?;
    let mut names = Vec::new();
    for item in items {
        let mut f = Cursor { t: item, i: 0 };
        let name = f.name()?;
        let args = match f.peek() == Some(&Token::LParen) {
            true => Some(function_arguments(&f.group()?)?),
            false => None,
        };
        if let Some(t) = f.peek() {
            return Err(Error::new(format!("syntax error at or near \"{t}\"")));
        }
        names.push(FunctionNamed { name, args });
    }
    Ok(Form::DropFunctions {
        names,
        if_exists,
        cascade,
    })
}

/// A function's arguments as a DROP lists them: each its type, after its mode and its name where
/// it writes them; an OUT argument none.
fn function_arguments(tokens: &[Token]) -> Result<Vec<DataType>> {
    let mut list = Cursor {
        t: tokens.to_vec(),
        i: 0,
    };
    let mut out = Vec::new();
    while !list.done() {
        let piece = list.item();
        let mut p = Cursor { t: piece, i: 0 };
        let out_only = p.kw("OUT");
        if !out_only {
            let _ = p.kw("INOUT") || p.kw("IN") || p.kw("VARIADIC");
        }
        let rest = &p.t[p.i..];
        let ty = Cursor::data_type_item(rest).or_else(|e| match rest.len() > 1 {
            true => Cursor::data_type_item(&rest[1..]),
            false => Err(e),
        })?;
        if !out_only {
            out.push(ty);
        }
    }
    Ok(out)
}

/// A type's name as PostgreSQL's TypeNameToString writes one a statement gives: a type the SQL
/// standard's words name as `pg_catalog`'s, any other as written, `[]` for an array.
fn type_name_written(dt: &DataType) -> String {
    use sqlparser::ast::TimezoneInfo as Tz;
    let standard = match dt {
        DataType::Array(sqlparser::ast::ArrayElemTypeDef::SquareBracket(e, _))
        | DataType::Array(sqlparser::ast::ArrayElemTypeDef::AngleBracket(e)) => {
            return format!("{}[]", type_name_written(e))
        }
        DataType::Int(_) | DataType::Integer(_) => "int4",
        DataType::SmallInt(_) => "int2",
        DataType::BigInt(_) => "int8",
        DataType::Real => "float4",
        DataType::DoublePrecision | DataType::Float(_) => "float8",
        DataType::Numeric(_) | DataType::Decimal(_) | DataType::Dec(_) => "numeric",
        DataType::Boolean => "bool",
        DataType::Char(_) | DataType::Character(_) => "bpchar",
        DataType::Varchar(_) | DataType::CharacterVarying(_) | DataType::CharVarying(_) => {
            "varchar"
        }
        DataType::Timestamp(_, Tz::Tz | Tz::WithTimeZone) => "timestamptz",
        DataType::Timestamp(..) => "timestamp",
        DataType::Time(_, Tz::Tz | Tz::WithTimeZone) => "timetz",
        DataType::Time(..) => "time",
        DataType::Interval { .. } => "interval",
        DataType::Bit(_) => "bit",
        DataType::BitVarying(_) => "varbit",
        DataType::JSON => "json",
        _ => return dt.to_string().to_lowercase(),
    };
    format!("pg_catalog.{standard}")
}

/// A type's name as a DROP names it, its schema and its shown form read off a name's parts; any
/// other spelling, one of the SQL standard's, shown as written.
fn type_named(item: &[Token]) -> TypeNamed {
    let words = text(item);
    let mut c = Cursor {
        t: item.to_vec(),
        i: 0,
    };
    let mut parts = Vec::new();
    while let Ok(p) = c.ident() {
        parts.push(p);
        if !c.token(Token::Period) {
            break;
        }
    }
    let mut array = false;
    while c.token(Token::LBracket) {
        if matches!(c.peek(), Some(Token::Number(..))) {
            c.i += 1;
        }
        array = c.token(Token::RBracket);
    }
    if !c.done() || parts.is_empty() {
        return TypeNamed {
            shown: words.clone(),
            words,
            schema: None,
        };
    }
    let schema = (parts.len() > 1).then(|| parts[..parts.len() - 1].join("."));
    let shown = format!("{}{}", parts.join("."), if array { "[]" } else { "" });
    TypeNamed {
        words,
        schema,
        shown,
    }
}

fn end(c: &Cursor, what: &str) -> Result<()> {
    if c.done() {
        Ok(())
    } else {
        Err(Error::new(format!("{what}: {} is not read", c.rest())))
    }
}

/// The object a COMMENT ON names, as its grammar names each kind the file makes.
#[derive(Clone, Debug)]
pub(crate) enum Commented {
    /// A TABLE, VIEW, INDEX or SEQUENCE, by its word, with its schema where the name gives one.
    Relation {
        word: &'static str,
        schema: Option<String>,
        own: String,
    },
    /// A COLUMN: its relation's name's parts, then its own.
    Column(Vec<String>),
    /// A TYPE, or a DOMAIN where it says so.
    Type { name: String, domain: bool },
    /// A FUNCTION, PROCEDURE or ROUTINE, by name, with its arguments' types where it lists them.
    Function {
        name: String,
        args: Option<Vec<sqlparser::ast::DataType>>,
    },
    /// A CONSTRAINT of a table, or of a DOMAIN where it says so.
    Constraint {
        name: String,
        on: String,
        domain: bool,
    },
    /// A RULE or a TRIGGER of a relation.
    OnRelation {
        word: &'static str,
        name: String,
        on: String,
    },
    Schema(String),
    Collation(String),
    /// A kind of object the file makes none of, by its words and why.
    Refused(String),
}

/// COMMENT ON: the object, by its kind's words, IS a text or NULL.
fn comment_on(c: &mut Cursor) -> Result<Form> {
    let what = "COMMENT ON";
    let server = |kind: &str| {
        Commented::Refused(format!(
            "{what} {kind} is not read: a {} is a running server's own, and a snail has no server",
            kind.to_lowercase()
        ))
    };
    let target = if c.kws(&["MATERIALIZED", "VIEW"]) || c.kws(&["FOREIGN", "TABLE"]) {
        let own = c.name()?;
        Commented::Refused(format!(
            "{what}: relation \"{own}\" does not exist, a snail holding no materialized view or \
             foreign table"
        ))
    } else if let Some(word) = ["TABLE", "VIEW", "INDEX", "SEQUENCE"]
        .into_iter()
        .find(|w| c.kw(w))
    {
        let first = c.ident()?;
        let (schema, own) = match c.token(Token::Period) {
            true => (Some(first), c.ident()?),
            false => (None, first),
        };
        Commented::Relation { word, schema, own }
    } else if c.kw("COLUMN") {
        let mut parts = vec![c.ident()?];
        while c.token(Token::Period) {
            parts.push(c.ident()?);
        }
        Commented::Column(parts)
    } else if c.kw("TYPE") || c.is("DOMAIN") {
        let domain = c.kw("DOMAIN");
        Commented::Type {
            name: c.name()?,
            domain,
        }
    } else if c.kw("FUNCTION") || c.kw("PROCEDURE") || c.kw("ROUTINE") {
        let name = c.name()?;
        let args = match c.peek() == Some(&Token::LParen) {
            true => Some(argument_types(&c.group()?)?),
            false => None,
        };
        Commented::Function { name, args }
    } else if c.kw("CONSTRAINT") {
        let name = c.ident()?;
        c.expect("ON")?;
        let domain = c.kw("DOMAIN");
        Commented::Constraint {
            name,
            on: c.name()?,
            domain,
        }
    } else if let Some(word) = ["RULE", "TRIGGER"].into_iter().find(|w| c.kw(w)) {
        let name = c.ident()?;
        c.expect("ON")?;
        Commented::OnRelation {
            word,
            name,
            on: c.name()?,
        }
    } else if c.kw("SCHEMA") {
        Commented::Schema(c.ident()?)
    } else if c.kw("COLLATION") {
        Commented::Collation(c.name()?)
    } else if let Some(kind) = ["DATABASE", "ROLE", "TABLESPACE", "SUBSCRIPTION", "PUBLICATION"]
        .into_iter()
        .find(|w| c.is(w))
    {
        c.kw(kind);
        c.name()?;
        server(kind)
    } else {
        let kind = c.rest();
        let kind = kind.split(" IS ").next().unwrap_or(&kind).to_string();
        return Err(Error::new(format!(
            "{what} {kind} is not read: the snail describes the objects its file makes, its \
             tables, columns, views, indexes, sequences, types, domains, functions, constraints, \
             rules, triggers, schemas and collations"
        )));
    };
    c.expect("IS")?;
    let text = match c.peek() {
        Some(Token::SingleQuotedString(s)) | Some(Token::EscapedStringLiteral(s)) => Some(s.clone()),
        Some(Token::DollarQuotedString(d)) => Some(d.value.clone()),
        Some(Token::Word(w)) if w.value.eq_ignore_ascii_case("NULL") && w.quote_style.is_none() => {
            None
        }
        _ => return Err(c.wanted("a quoted string or NULL")),
    };
    c.i += 1;
    end(c, what)?;
    Ok(Form::Comment { target, text })
}

/// The types of a function's arguments as a COMMENT ON lists them, each read as a data type.
fn argument_types(tokens: &[Token]) -> Result<Vec<sqlparser::ast::DataType>> {
    let mut out = Vec::new();
    let mut piece: Vec<Token> = Vec::new();
    let mut depth = 0;
    let mut pieces = Vec::new();
    for t in tokens {
        match t {
            Token::LParen | Token::LBracket => depth += 1,
            Token::RParen | Token::RBracket => depth -= 1,
            Token::Comma if depth == 0 => {
                pieces.push(std::mem::take(&mut piece));
                continue;
            }
            _ => {}
        }
        piece.push(t.clone());
    }
    if piece.iter().any(|t| !matches!(t, Token::Whitespace(_))) {
        pieces.push(piece);
    }
    for p in pieces {
        let dt = Parser::new(&PostgreSqlDialect {})
            .with_tokens(p)
            .parse_data_type()
            .map_err(|e| Error::new(format!("COMMENT ON FUNCTION: an argument is no type ({e})")))?;
        out.push(dt);
    }
    Ok(out)
}

/// A CREATE VIEW's tokens without the WITH [CASCADED | LOCAL] CHECK OPTION that ends it, and that
/// option as PostgreSQL's reloptions write it, CASCADED where it names neither.
fn check_option(tokens: &[Token]) -> (Vec<Token>, Option<&'static str>) {
    let words: Vec<usize> = (0..tokens.len())
        .filter(|&i| !matches!(tokens[i], Token::Whitespace(_) | Token::SemiColon))
        .collect();
    let word = |i: usize, w: &str| {
        matches!(&tokens[i], Token::Word(x) if x.quote_style.is_none() && x.value.eq_ignore_ascii_case(w))
    };
    let n = words.len();
    if n < 4 || !word(words[n - 1], "OPTION") || !word(words[n - 2], "CHECK") {
        return (tokens.to_vec(), None);
    }
    let (start, option) = if word(words[n - 3], "WITH") {
        (words[n - 3], "cascaded")
    } else if word(words[n - 4], "WITH") && word(words[n - 3], "CASCADED") {
        (words[n - 4], "cascaded")
    } else if word(words[n - 4], "WITH") && word(words[n - 3], "LOCAL") {
        (words[n - 4], "local")
    } else {
        return (tokens.to_vec(), None);
    };
    (tokens[..start].to_vec(), Some(option))
}

/// Whether an expression is a constant written out: a literal, signed or cast, or a typed string.
fn constant(e: &Expr) -> bool {
    match e {
        Expr::Value(_) | Expr::TypedString(_) => true,
        Expr::UnaryOp { expr, .. } | Expr::Nested(expr) | Expr::Cast { expr, .. } => constant(expr),
        _ => false,
    }
}

/// The multirange PostgreSQL names for a range where its DDL names none: "multi" put before the
/// first "range" in its name, or "_multirange" after it.
pub(crate) fn multirange_name(range: &str) -> String {
    let (schema, bare) = match range.rsplit_once('.') {
        Some((s, b)) => (Some(s), b),
        None => (None, range),
    };
    let name = match bare.find("range") {
        Some(at) => format!("{}multi{}", &bare[..at], &bare[at..]),
        None => format!("{bare}_multirange"),
    };
    match schema {
        Some(s) => format!("{s}.{name}"),
        None => name,
    }
}

fn create_type(c: &mut Cursor) -> Result<Form> {
    let name = c.name()?;
    if c.done() {
        return Err(Error::new(format!(
            "CREATE TYPE {name}: a shell type stands for a base type, written in C, which a snail does not run"
        )));
    }
    if c.is("(") || c.peek() == Some(&Token::LParen) {
        return Err(Error::new(format!(
            "CREATE TYPE {name} (…): a base type is written in C, which a snail does not run"
        )));
    }
    c.expect("AS")?;
    let kind = if c.kw("ENUM") {
        let mut labels: Vec<String> = Vec::new();
        for item in c.items()? {
            let mut i = Cursor { t: item, i: 0 };
            let label = i.string()?;
            end(&i, &format!("{name}'s label"))?;
            if labels.contains(&label) {
                return Err(Error::new(format!(
                    "{name}: the label \"{label}\" is written twice"
                )));
            }
            labels.push(label);
        }
        TypeKind::Enum { labels }
    } else if c.kw("RANGE") {
        let mut subtype = None;
        let mut range = crate::catalog::RangeType::default();
        for item in c.items()? {
            let mut i = Cursor { t: item, i: 0 };
            let key = i.ident()?;
            i.expect_token(Token::Eq)?;
            match key.as_str() {
                "subtype" => subtype = Some(format_type(&Cursor::data_type_item(&i.t[i.i..])?)),
                "subtype_opclass" => range.opclass = Some(i.name()?),
                "collation" => range.collation = Some(i.name()?),
                "canonical" => range.canonical = Some(i.name()?),
                "subtype_diff" => range.subtype_diff = Some(i.name()?),
                "multirange_type_name" => range.multirange = i.name()?,
                other => {
                    return Err(Error::new(format!(
                        "CREATE TYPE {name} AS RANGE: {other} is no option of a range"
                    )))
                }
            }
            if key != "subtype" {
                end(&i, &format!("{name}'s {key}"))?;
            }
        }
        range.subtype = subtype.ok_or_else(|| {
            Error::new(format!(
                "CREATE TYPE {name} AS RANGE: its SUBTYPE is wanted"
            ))
        })?;
        if range.multirange.is_empty() {
            range.multirange = multirange_name(&name);
        } else if !range.multirange.contains('.') {
            if let Some((schema, _)) = name.rsplit_once('.') {
                range.multirange = format!("{schema}.{}", range.multirange);
            }
        }
        TypeKind::Range(range)
    } else if c.peek() == Some(&Token::LParen) {
        let mut fields = Vec::new();
        for item in c.items()? {
            if item.is_empty() {
                continue;
            }
            let mut i = Cursor { t: item, i: 0 };
            let field = i.ident()?;
            let dt = i.data_type()?;
            let collation = if i.kw("COLLATE") {
                Some(i.name()?)
            } else {
                None
            };
            end(&i, &format!("{name}.{field}"))?;
            fields.push((field, format_type(&dt), collation));
        }
        TypeKind::Composite { fields }
    } else {
        return Err(Error::new(format!(
            "CREATE TYPE {name} AS {}: only ENUM, RANGE and a composite are read",
            c.rest()
        )));
    };
    end(c, &format!("CREATE TYPE {name}"))?;
    Ok(Form::CreateType(Type { name, kind }))
}

fn alter_type(c: &mut Cursor) -> Result<Form> {
    let name = c.name()?;
    let change = if c.kws(&["ADD", "VALUE"]) {
        let if_not_exists = c.kws(&["IF", "NOT", "EXISTS"]);
        let label = c.string()?;
        let beside = if c.kw("BEFORE") {
            Some((true, c.string()?))
        } else if c.kw("AFTER") {
            Some((false, c.string()?))
        } else {
            None
        };
        AlterType::AddValue {
            label,
            if_not_exists,
            beside,
        }
    } else if c.kws(&["RENAME", "VALUE"]) {
        let from = c.string()?;
        c.expect("TO")?;
        AlterType::RenameValue {
            from,
            to: c.string()?,
        }
    } else {
        return Err(Error::new(format!(
            "ALTER TYPE {name} {}: only ADD VALUE and RENAME VALUE are read",
            c.rest()
        )));
    };
    end(c, &format!("ALTER TYPE {name}"))?;
    Ok(Form::AlterType(name, change))
}

/// The keywords that end a DEFAULT's expression in a domain or a column, as PostgreSQL's b_expr
/// does: an expression there holds no NOT, NULL, CHECK or constraint of its own.
const AFTER_DEFAULT: [&str; 12] = [
    "CONSTRAINT",
    "NOT",
    "NULL",
    "CHECK",
    "COLLATE",
    "DEFAULT",
    "PRIMARY",
    "UNIQUE",
    "REFERENCES",
    "GENERATED",
    "DEFERRABLE",
    "INITIALLY",
];

fn create_domain(c: &mut Cursor) -> Result<Form> {
    let name = c.name()?;
    c.kw("AS");
    let dt = c.data_type()?;
    let mut d = Domain {
        name: name.clone(),
        base: format_type(&dt),
        base_type: crate::catalog::held_type(&dt).ok(),
        collation: None,
        default: None,
        not_null: false,
        not_null_name: None,
        checks: Vec::new(),
    };
    let mut nullness: Option<bool> = None;
    while !c.done() {
        if c.kw("COLLATE") {
            d.collation = Some(c.name()?);
            continue;
        }
        if c.kw("DEFAULT") {
            if d.default.is_some() {
                return Err(Error::new(format!(
                    "CREATE DOMAIN {name}: multiple default expressions"
                )));
            }
            d.default = Some(c.expr_until(&AFTER_DEFAULT)?);
            continue;
        }
        let constraint = if c.kw("CONSTRAINT") {
            Some(c.ident()?)
        } else {
            None
        };
        if c.kws(&["NOT", "NULL"]) {
            if nullness == Some(false) {
                return Err(Error::new(format!(
                    "CREATE DOMAIN {name}: conflicting NULL/NOT NULL constraints"
                )));
            }
            nullness = Some(true);
            d.not_null = true;
            d.not_null_name = constraint;
        } else if c.kw("NULL") {
            if nullness == Some(true) {
                return Err(Error::new(format!(
                    "CREATE DOMAIN {name}: conflicting NULL/NOT NULL constraints"
                )));
            }
            nullness = Some(false);
        } else if c.kw("CHECK") {
            let check = expression(&c.group()?)?;
            if c.kws(&["NO", "INHERIT"]) {
                return Err(Error::new(format!(
                    "CREATE DOMAIN {name}: a domain's CHECK is never NO INHERIT"
                )));
            }
            d.checks.push(DomainCheck {
                name: constraint.unwrap_or_default(),
                check,
                valid: true,
            });
        } else {
            return Err(Error::new(format!(
                "CREATE DOMAIN {name}: {} is not read: a domain takes COLLATE, DEFAULT, NOT NULL, \
                 NULL and CHECK",
                c.rest()
            )));
        }
    }
    Ok(Form::CreateDomain(d))
}

fn alter_domain(c: &mut Cursor) -> Result<Form> {
    let name = c.name()?;
    let change = if c.kws(&["SET", "DEFAULT"]) {
        AlterDomain::SetDefault(c.expr_until(&[])?)
    } else if c.kws(&["DROP", "DEFAULT"]) {
        AlterDomain::DropDefault
    } else if c.kws(&["SET", "NOT", "NULL"]) {
        AlterDomain::SetNotNull
    } else if c.kws(&["DROP", "NOT", "NULL"]) {
        AlterDomain::DropNotNull
    } else if c.kw("ADD") {
        let constraint = if c.kw("CONSTRAINT") {
            Some(c.ident()?)
        } else {
            None
        };
        if c.kw("CHECK") {
            let check = expression(&c.group()?)?;
            let valid = !c.kws(&["NOT", "VALID"]);
            AlterDomain::AddCheck {
                name: constraint,
                check,
                valid,
            }
        } else if c.kws(&["NOT", "NULL"]) {
            AlterDomain::AddNotNull { name: constraint }
        } else {
            return Err(Error::new(format!(
                "ALTER DOMAIN {name} ADD {}: a domain's constraint is a CHECK or NOT NULL",
                c.rest()
            )));
        }
    } else if c.kws(&["DROP", "CONSTRAINT"]) {
        let if_exists = c.kws(&["IF", "EXISTS"]);
        let constraint = c.ident()?;
        if !c.kw("RESTRICT") {
            c.kw("CASCADE");
        }
        AlterDomain::DropConstraint {
            name: constraint,
            if_exists,
        }
    } else if c.kws(&["RENAME", "CONSTRAINT"]) {
        let from = c.ident()?;
        c.expect("TO")?;
        AlterDomain::RenameConstraint {
            from,
            to: c.ident()?,
        }
    } else if c.kws(&["VALIDATE", "CONSTRAINT"]) {
        AlterDomain::ValidateConstraint(c.ident()?)
    } else {
        return Err(Error::new(format!(
            "ALTER DOMAIN {name} {}: only its default, NOT NULL and its constraints are read",
            c.rest()
        )));
    };
    end(c, &format!("ALTER DOMAIN {name}"))?;
    Ok(Form::AlterDomain(name, change))
}

/// The options of a sequence, until the end or a closing parenthesis. `identity` reads SEQUENCE
/// NAME, which only an identity column's options may give.
pub(crate) fn sequence_options(
    c: &mut Cursor,
    what: &str,
    identity: bool,
) -> Result<Vec<SequenceOption>> {
    let mut out = Vec::new();
    while !c.done() {
        let o = if c.kw("AS") {
            let dt = c.data_type()?;
            SequenceOption::As(SequenceType::of(&dt).ok_or_else(|| {
                Error::new(format!(
                    "{what}: sequence type must be smallint, integer, or bigint"
                ))
            })?)
        } else if c.kw("INCREMENT") {
            c.kw("BY");
            SequenceOption::Increment(c.integer()?)
        } else if c.kw("MINVALUE") {
            SequenceOption::MinValue(Some(c.integer()?))
        } else if c.kws(&["NO", "MINVALUE"]) {
            SequenceOption::MinValue(None)
        } else if c.kw("MAXVALUE") {
            SequenceOption::MaxValue(Some(c.integer()?))
        } else if c.kws(&["NO", "MAXVALUE"]) {
            SequenceOption::MaxValue(None)
        } else if c.kw("START") {
            c.kw("WITH");
            SequenceOption::Start(c.integer()?)
        } else if c.kw("RESTART") {
            let with = c.kw("WITH");
            if with
                || matches!(
                    c.peek(),
                    Some(Token::Number(..) | Token::Minus | Token::Plus)
                )
            {
                SequenceOption::Restart(Some(c.integer()?))
            } else {
                SequenceOption::Restart(None)
            }
        } else if c.kw("CACHE") {
            SequenceOption::Cache(c.integer()?)
        } else if c.kw("CYCLE") {
            SequenceOption::Cycle(true)
        } else if c.kws(&["NO", "CYCLE"]) {
            SequenceOption::Cycle(false)
        } else if c.kws(&["OWNED", "BY"]) {
            if c.kw("NONE") {
                SequenceOption::OwnedBy(None)
            } else {
                let full = c.name()?;
                let (table, column) = full.rsplit_once('.').ok_or_else(|| {
                    Error::new(format!("{what}: OWNED BY names a table's column"))
                })?;
                SequenceOption::OwnedBy(Some((table.to_string(), column.to_string())))
            }
        } else if identity && c.kws(&["SEQUENCE", "NAME"]) {
            SequenceOption::SequenceName(c.name()?)
        } else if c.is("LOGGED") || c.is("UNLOGGED") {
            return Err(Error::new(format!(
                "{what}: {} is not read: a snail's sequences are kept in its file",
                c.rest()
            )));
        } else {
            return Err(Error::new(format!(
                "{what}: {} is no option of a sequence the snail reads",
                c.rest()
            )));
        };
        out.push(o);
    }
    Ok(out)
}

fn create_sequence(c: &mut Cursor) -> Result<Form> {
    let if_not_exists = c.kws(&["IF", "NOT", "EXISTS"]);
    let name = c.name()?;
    let what = format!("CREATE SEQUENCE {name}");
    let options = sequence_options(c, &what, false)?;
    let sequence = Sequence::made(&name, &options)?;
    Ok(Form::CreateSequence {
        sequence,
        if_not_exists,
        options,
    })
}

fn alter_sequence(c: &mut Cursor) -> Result<Form> {
    let if_exists = c.kws(&["IF", "EXISTS"]);
    let name = c.name()?;
    if c.is("RENAME") || c.is("OWNER") || (c.is("SET") && !c.is("SEQUENCE")) {
        return Err(Error::new(format!(
            "ALTER SEQUENCE {name} {}: only a sequence's options are read",
            c.rest()
        )));
    }
    let options = sequence_options(c, &format!("ALTER SEQUENCE {name}"), false)?;
    Ok(Form::AlterSequence {
        name,
        if_exists,
        options,
    })
}

fn create_collation(c: &mut Cursor) -> Result<Form> {
    let if_not_exists = c.kws(&["IF", "NOT", "EXISTS"]);
    let name = c.name()?;
    let mut k = Collation {
        name: name.clone(),
        provider: "libc".into(),
        locale: None,
        lc_collate: None,
        lc_ctype: None,
        deterministic: true,
        rules: None,
        from: None,
    };
    if c.kw("FROM") {
        k.from = Some(c.name()?);
    } else {
        for item in c.items()? {
            let mut i = Cursor { t: item, i: 0 };
            let key = i.ident()?;
            i.expect_token(Token::Eq)?;
            let value = i.option_value()?;
            end(&i, &format!("CREATE COLLATION {name}'s {key}"))?;
            match key.as_str() {
                "provider" => k.provider = value.to_lowercase(),
                "locale" => k.locale = Some(value),
                "lc_collate" => k.lc_collate = Some(value),
                "lc_ctype" => k.lc_ctype = Some(value),
                "deterministic" => {
                    k.deterministic = match value.to_lowercase().as_str() {
                        "true" | "on" | "yes" | "1" => true,
                        "false" | "off" | "no" | "0" => false,
                        _ => {
                            return Err(Error::new(format!(
                            "CREATE COLLATION {name}: deterministic is true or false, not {value}"
                        )))
                        }
                    }
                }
                "rules" => k.rules = Some(value),
                other => {
                    return Err(Error::new(format!(
                        "CREATE COLLATION {name}: {other} is not read"
                    )))
                }
            }
        }
    }
    end(c, &format!("CREATE COLLATION {name}"))?;
    Ok(Form::CreateCollation {
        collation: k,
        if_not_exists,
    })
}

/// A collation CREATE COLLATION makes, checked as PostgreSQL checks it: one of byte order as it is;
/// one that calls a library only with the feature `collations`, an ICU locale given in the
/// standard form of a language tag, as PostgreSQL stores it, with its notice.
fn made_collation(k: &mut Collation, declared: &[Collation]) -> Result<Vec<String>> {
    if k.bytes_casing(declared).is_some() {
        return Ok(Vec::new());
    }
    if let Some(f) = &k.from {
        crate::catalog::collation_casing(f, declared)
            .map_err(|e| Error::new(format!("CREATE COLLATION {}: {e}", k.name)))?;
        return Ok(Vec::new());
    }
    if !k.deterministic && k.provider != "icu" {
        return Err(Error::new(
            "nondeterministic collations not supported with this provider",
        ));
    }
    if k.rules.is_some() && k.provider != "icu" {
        return Err(Error::new(
            "ICU rules cannot be specified unless locale provider is ICU",
        ));
    }
    if !cfg!(feature = "collations") {
        return Err(Error::new(format!(
            "CREATE COLLATION {}: {}",
            k.name,
            crate::catalog::NOT_HELD
        )));
    }
    let mut notices = Vec::new();
    if k.provider == "icu" {
        let locale = k
            .locale
            .clone()
            .ok_or_else(|| Error::new("parameter \"locale\" must be specified"))?;
        let tag = crate::collations::language_tag(&locale)?;
        if tag != locale {
            notices.push(format!(
                "using standard form \"{tag}\" for ICU locale \"{locale}\""
            ));
            k.locale = Some(tag);
        }
    }
    let spec = k
        .spec(declared)
        .ok_or_else(|| Error::new(format!("CREATE COLLATION {}: {}", k.name, NOT_READ)))?;
    crate::collations::open(&spec, &k.name)?;
    Ok(notices)
}

/// Why a collation of a provider the snail does not read is refused.
const NOT_READ: &str = "its provider is not read";

/// A FOREIGN KEY as written, before its names are found: in a column's REFERENCES, a table's
/// FOREIGN KEY, or ALTER TABLE … ADD.
#[derive(Clone, Debug)]
pub(crate) struct KeyReference {
    pub name: Option<String>,
    pub columns: Vec<String>,
    /// Its last column written PERIOD.
    pub period: bool,
    pub parent: String,
    /// The parent's columns, where written; else its primary key's.
    pub parent_columns: Option<Vec<String>>,
    /// The parent's last column written PERIOD.
    pub parent_period: bool,
    pub full: bool,
    /// Each action as PostgreSQL's catalog codes it (`a`, `r`, `c`, `n`, `d`), with the columns
    /// SET NULL or SET DEFAULT lists.
    pub on_delete: (char, Vec<String>),
    pub on_update: (char, Vec<String>),
    pub deferrable: bool,
    pub deferred: bool,
    pub enforced: bool,
    pub not_valid: bool,
}

/// The attributes written after a constraint, as PostgreSQL's grammar reads them: [NOT]
/// DEFERRABLE, INITIALLY DEFERRED or IMMEDIATE, [NOT] ENFORCED, and where `table`, NOT VALID and
/// NO INHERIT. Returns (deferrable, deferred, enforced, not valid), each as written.
fn constraint_attributes(
    c: &mut Cursor,
    table: bool,
    what: &str,
) -> Result<(Option<bool>, Option<bool>, Option<bool>, bool)> {
    let (mut deferrable, mut deferred, mut enforced, mut not_valid) = (None, None, None, false);
    let set = |slot: &mut Option<bool>, v: bool| -> Result<()> {
        if slot.is_some_and(|s| s != v) {
            return Err(Error::new(format!(
                "{what}: conflicting constraint properties"
            )));
        }
        *slot = Some(v);
        Ok(())
    };
    loop {
        if c.kws(&["NOT", "DEFERRABLE"]) {
            set(&mut deferrable, false)?;
        } else if c.kw("DEFERRABLE") {
            set(&mut deferrable, true)?;
        } else if c.kws(&["INITIALLY", "DEFERRED"]) {
            set(&mut deferred, true)?;
        } else if c.kws(&["INITIALLY", "IMMEDIATE"]) {
            set(&mut deferred, false)?;
        } else if c.kws(&["NOT", "ENFORCED"]) {
            set(&mut enforced, false)?;
        } else if c.kw("ENFORCED") {
            set(&mut enforced, true)?;
        } else if table && c.kws(&["NOT", "VALID"]) {
            not_valid = true;
        } else if table && c.kws(&["NO", "INHERIT"]) {
            return Err(Error::new(format!(
                "{what}: FOREIGN KEY constraints cannot be marked NO INHERIT"
            )));
        } else {
            break;
        }
        if deferrable == Some(false) && deferred == Some(true) {
            return Err(Error::new(format!(
                "{what}: constraint declared INITIALLY DEFERRED must be DEFERRABLE"
            )));
        }
    }
    Ok((deferrable, deferred, enforced, not_valid))
}

/// One referential action, after ON DELETE or ON UPDATE: its code, and the columns SET NULL or SET
/// DEFAULT lists, only ON DELETE's.
fn referential_action(c: &mut Cursor, delete: bool, what: &str) -> Result<(char, Vec<String>)> {
    let code = if c.kws(&["NO", "ACTION"]) {
        'a'
    } else if c.kw("RESTRICT") {
        'r'
    } else if c.kw("CASCADE") {
        'c'
    } else if c.kws(&["SET", "NULL"]) {
        'n'
    } else if c.kws(&["SET", "DEFAULT"]) {
        'd'
    } else {
        return Err(c.wanted("NO ACTION, RESTRICT, CASCADE, SET NULL or SET DEFAULT"));
    };
    let mut columns = Vec::new();
    if matches!(code, 'n' | 'd') && c.peek() == Some(&Token::LParen) {
        let set = if code == 'n' {
            "SET NULL"
        } else {
            "SET DEFAULT"
        };
        if !delete {
            return Err(Error::new(format!(
                "{what}: a column list with {set} is only supported for ON DELETE actions"
            )));
        }
        let mut g = Cursor {
            t: c.group()?,
            i: 0,
        };
        loop {
            columns.push(g.ident()?);
            if !g.token(Token::Comma) {
                break;
            }
        }
        end(&g, what)?;
    }
    Ok((code, columns))
}

/// A foreign key's REFERENCES and what follows it, the cursor standing at REFERENCES: the parent,
/// its columns, MATCH, the actions and the attributes. `table` where it is a table's constraint.
fn key_reference(
    c: &mut Cursor,
    name: Option<String>,
    (columns, period): (Vec<String>, bool),
    table: bool,
    what: &str,
) -> Result<KeyReference> {
    c.expect("REFERENCES")?;
    let parent = c.name()?;
    let (parent_columns, parent_period) = if c.peek() == Some(&Token::LParen) {
        let (out, period) = period_columns(c, what)?;
        (Some(out), period)
    } else {
        (None, false)
    };
    let full = if c.kw("MATCH") {
        if c.kw("FULL") {
            true
        } else if c.kw("SIMPLE") {
            false
        } else if c.kw("PARTIAL") {
            return Err(Error::new(format!(
                "{what}: MATCH PARTIAL not yet implemented"
            )));
        } else {
            return Err(c.wanted("FULL, PARTIAL or SIMPLE"));
        }
    } else {
        false
    };
    let (mut on_delete, mut on_update) = (None, None);
    loop {
        if on_delete.is_none() && c.kws(&["ON", "DELETE"]) {
            on_delete = Some(referential_action(c, true, what)?);
        } else if on_update.is_none() && c.kws(&["ON", "UPDATE"]) {
            on_update = Some(referential_action(c, false, what)?);
        } else {
            break;
        }
    }
    let (deferrable, deferred, enforced, not_valid) = constraint_attributes(c, table, what)?;
    let deferred = deferred.unwrap_or(false);
    Ok(KeyReference {
        name,
        columns,
        period,
        parent,
        parent_columns,
        parent_period,
        full,
        on_delete: on_delete.unwrap_or(('a', Vec::new())),
        on_update: on_update.unwrap_or(('a', Vec::new())),
        // INITIALLY DEFERRED makes it DEFERRABLE, as PostgreSQL's grammar does.
        deferrable: deferrable.unwrap_or(false) || deferred,
        deferred,
        enforced: enforced.unwrap_or(true),
        not_valid,
    })
}

/// A table's FOREIGN KEY constraint, the cursor standing at its first word: [CONSTRAINT name]
/// FOREIGN KEY (columns) REFERENCES …; none where it is another constraint.
fn table_key_reference(c: &mut Cursor, what: &str) -> Result<Option<KeyReference>> {
    let start = c.i;
    let name = if c.kw("CONSTRAINT") {
        Some(c.ident()?)
    } else {
        None
    };
    if !c.kws(&["FOREIGN", "KEY"]) {
        c.i = start;
        return Ok(None);
    }
    let columns = period_columns(c, what)?;
    key_reference(c, name, columns, true, what).map(Some)
}

/// A foreign key's columns in parentheses, the cursor standing at the parenthesis, as PostgreSQL's
/// grammar reads them: one or more, and PERIOD before the last where there are two or more.
/// Returns the columns, and whether the last is PERIOD.
fn period_columns(c: &mut Cursor, what: &str) -> Result<(Vec<String>, bool)> {
    let mut g = Cursor {
        t: c.group()?,
        i: 0,
    };
    let mut columns = Vec::new();
    let mut period = false;
    loop {
        // PERIOD before a name; a column named period stands alone.
        if g.is("PERIOD") && matches!(g.peek_at(1), Some(Token::Word(_))) {
            if columns.is_empty() {
                return Err(Error::new(format!(
                    "{what}: PERIOD is read only before the last of two or more columns"
                )));
            }
            g.i += 1;
            period = true;
        }
        columns.push(g.ident()?);
        if !g.token(Token::Comma) {
            break;
        }
        if period {
            return Err(Error::new(format!(
                "{what}: PERIOD is read only before the last of two or more columns"
            )));
        }
    }
    end(&g, what)?;
    Ok((columns, period))
}

/// What a column of a CREATE TABLE writes that sqlparser 0.63 does not read, taken out of its
/// tokens: its identity and the sequence's options, its generation expression, and NULLS [NOT]
/// DISTINCT on its UNIQUE.
#[derive(Default)]
pub(crate) struct ColumnExtras {
    pub identity: Option<(bool, Vec<SequenceOption>)>,
    pub generated: Option<Generated>,
    pub unique_nulls_distinct: Option<bool>,
}

/// A CREATE TABLE's tokens with what sqlparser does not read taken out, what each column wrote, its
/// foreign keys, in the order written, and which of its PRIMARY KEYs and UNIQUEs, counted in the
/// order written, are WITHOUT OVERLAPS.
pub(crate) type TableExtras = (
    Vec<Token>,
    Vec<(String, ColumnExtras)>,
    Vec<KeyReference>,
    Vec<usize>,
);

/// A CREATE INDEX's tokens with its operator classes' parameters taken out, and those, by key.
pub(crate) type IndexExtras = (Vec<Token>, Vec<(usize, String)>);

/// A CREATE TABLE's tokens with what sqlparser does not read taken out, and what was taken, by
/// column name. Refuses by name the forms of a key or a column the snail does not hold.
pub(crate) fn create_table_extras(tokens: &[Token]) -> Result<Option<TableExtras>> {
    let c = Cursor::new(tokens);
    let mut head = Cursor {
        t: c.t.clone(),
        i: 0,
    };
    if !head.kw("CREATE") {
        return Ok(None);
    }
    for kind in ["TEMP", "TEMPORARY", "UNLOGGED", "GLOBAL", "LOCAL"] {
        if head.is(kind) {
            return Err(Error::new(format!(
                "CREATE {kind} TABLE: a snail's tables are kept in its file, each one logged and lasting"
            )));
        }
    }
    if !head.kw("TABLE") {
        return Ok(None);
    }
    head.kws(&["IF", "NOT", "EXISTS"]);
    let table = head.name()?;
    let open = head.i;
    if head.peek() != Some(&Token::LParen) {
        return Ok(None);
    }
    let elements = head.items()?;
    let close = head.i;
    let mut extras = Vec::new();
    let mut references = Vec::new();
    let mut kept_elements: Vec<Vec<Token>> = Vec::new();
    let (mut keys, mut without_overlaps) = (0, Vec::new());
    for element in elements {
        let mut e = Cursor { t: element, i: 0 };
        let first = matches!(e.peek(), Some(Token::Word(w)) if w.quote_style.is_none()
            && ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN", "EXCLUDE", "LIKE"]
                .iter()
                .any(|k| w.value.eq_ignore_ascii_case(k)));
        if first {
            if let Some(r) = table_key_reference(&mut e, &table)? {
                end(&e, &format!("{table}'s FOREIGN KEY"))?;
                references.push(r);
                continue;
            }
            // A PRIMARY KEY or a UNIQUE whose last column is WITHOUT OVERLAPS: the two words taken
            // out, and the key counted among the table's PRIMARY KEYs and UNIQUEs.
            let mut k = Cursor {
                t: e.t.clone(),
                i: 0,
            };
            if k.kw("CONSTRAINT") {
                k.ident()?;
            }
            let keyed = k.kws(&["PRIMARY", "KEY"]) || k.kw("UNIQUE");
            let written = e.t.windows(2).position(|w| {
                Cursor::word_is(Some(&w[0]), "WITHOUT") && Cursor::word_is(Some(&w[1]), "OVERLAPS")
            });
            if let Some(at) = written {
                let closes = keyed && {
                    let _ = k.kws(&["NULLS", "NOT", "DISTINCT"]) || k.kws(&["NULLS", "DISTINCT"]);
                    k.peek() == Some(&Token::LParen) && k.group().is_ok() && at + 3 == k.i
                };
                if !closes {
                    return Err(Error::new(format!(
                        "{table}: WITHOUT OVERLAPS is read only after the last column of a \
                         PRIMARY KEY's or a UNIQUE's columns"
                    )));
                }
                without_overlaps.push(keys);
                let mut t = e.t;
                t.drain(at..at + 2);
                kept_elements.push(t);
            } else {
                kept_elements.push(e.t);
            }
            keys += usize::from(keyed);
            continue;
        }
        let column = e.ident()?;
        let mut x = ColumnExtras::default();
        let mut out: Vec<Token> = e.t[..e.i].to_vec();
        while !e.done() {
            // A REFERENCES, named or not, with its MATCH, actions and attributes.
            if e.is("REFERENCES")
                || (e.is("CONSTRAINT") && Cursor::word_is(e.peek_at(2), "REFERENCES"))
            {
                let name = if e.kw("CONSTRAINT") {
                    Some(e.ident()?)
                } else {
                    None
                };
                let what = format!("{table}.{column}");
                references.push(key_reference(
                    &mut e,
                    name,
                    (vec![column.clone()], false),
                    false,
                    &what,
                )?);
                continue;
            }
            if e.kws(&["GENERATED", "ALWAYS", "AS", "IDENTITY"])
                || e.kws(&["GENERATED", "BY", "DEFAULT", "AS", "IDENTITY"])
            {
                let always = Cursor::word_is(e.t.get(e.i - 3), "ALWAYS");
                let options = if e.peek() == Some(&Token::LParen) {
                    let inner = e.group()?;
                    let mut o = Cursor { t: inner, i: 0 };
                    sequence_options(&mut o, &format!("{table}.{column}'s identity"), true)?
                } else {
                    Vec::new()
                };
                x.identity = Some((always, options));
                continue;
            }
            if e.kws(&["GENERATED", "ALWAYS", "AS"]) {
                let expr = expression(&e.group()?)?;
                let stored = if e.kw("STORED") {
                    true
                } else {
                    e.kw("VIRTUAL");
                    false
                };
                x.generated = Some(Generated { expr, stored });
                continue;
            }
            if e.kw("UNIQUE") {
                out.push(e.t[e.i - 1].clone());
                if e.kws(&["NULLS", "NOT", "DISTINCT"]) {
                    x.unique_nulls_distinct = Some(false);
                } else if e.kws(&["NULLS", "DISTINCT"]) {
                    x.unique_nulls_distinct = Some(true);
                }
                continue;
            }
            if e.is("NATIONAL")
                && (Cursor::word_is(e.peek_at(1), "CHARACTER")
                    || Cursor::word_is(e.peek_at(1), "CHAR"))
            {
                // PostgreSQL reads NATIONAL CHARACTER as CHARACTER.
                e.i += 1;
                continue;
            }
            if e.peek() == Some(&Token::LParen) {
                let start = e.i;
                e.group()?;
                out.extend_from_slice(&e.t[start..e.i]);
                continue;
            }
            out.push(e.t[e.i].clone());
            e.i += 1;
        }
        kept_elements.push(out);
        extras.push((column, x));
    }
    let mut kept: Vec<Token> = c.t[..open + 1].to_vec();
    for (n, element) in kept_elements.into_iter().enumerate() {
        if n > 0 {
            kept.push(Token::Comma);
        }
        kept.extend(element);
    }
    kept.extend_from_slice(&c.t[close - 1..]);
    Ok(Some((kept, extras, references, without_overlaps)))
}

/// A CREATE INDEX's tokens with each operator class's parameters taken out, and those taken, by
/// the position of the key they follow.
pub(crate) fn index_extras(tokens: &[Token]) -> Result<IndexExtras> {
    let c = Cursor::new(tokens);
    let mut head = Cursor {
        t: c.t.clone(),
        i: 0,
    };
    // The key list is the first parenthesis after ON's table.
    while !head.done() && !head.is("ON") {
        head.i += 1;
    }
    if head.done() {
        return Ok((c.t, Vec::new()));
    }
    head.i += 1;
    head.kw("ONLY");
    head.name()?;
    if head.kw("USING") {
        head.ident()?;
    }
    if head.peek() != Some(&Token::LParen) {
        return Ok((c.t, Vec::new()));
    }
    let open = head.i;
    let keys = head.items()?;
    let close = head.i;
    let mut kept: Vec<Token> = c.t[..open + 1].to_vec();
    let mut params = Vec::new();
    for (n, key) in keys.into_iter().enumerate() {
        if n > 0 {
            kept.push(Token::Comma);
        }
        // An operator class's parameters: a parenthesis after a name, past the key's expression.
        let mut k = Cursor { t: key, i: 0 };
        let mut out = Vec::new();
        while !k.done() {
            let after_word = k.i > 0 && matches!(k.t.get(k.i - 1), Some(Token::Word(_)));
            if k.peek() == Some(&Token::LParen) && after_word && !out.is_empty() && k.i > 1 {
                let start = k.i;
                let inner = k.group()?;
                // A function's arguments follow its name with nothing between; an operator class
                // follows the key's expression, so a parenthesis after a word that follows another
                // word or a closing parenthesis is the operator class's.
                let before = &k.t[start - 2];
                if matches!(before, Token::Word(_) | Token::RParen) {
                    params.push((n, text(&inner)));
                    continue;
                }
                out.extend_from_slice(&k.t[start..k.i]);
                continue;
            }
            out.push(k.t[k.i].clone());
            k.i += 1;
        }
        kept.extend(out);
    }
    kept.extend_from_slice(&c.t[close - 1..]);
    Ok((kept, params))
}

/// Whether a statement's first words, lower case, are CREATE [OR REPLACE] FUNCTION or PROCEDURE.
fn routine(words: &[String]) -> bool {
    let made = |x: Option<&String>| matches!(x.map(String::as_str), Some("function" | "procedure"));
    words.first().map(String::as_str) == Some("create")
        && (made(words.get(1))
            || words.get(1).map(String::as_str) == Some("or")
                && words.get(2).map(String::as_str) == Some("replace")
                && made(words.get(3)))
}

/// The statements of a DDL, split on their semicolons by sqlparser's PostgreSQL tokenizer.
pub(crate) fn statements(ddl: &str) -> Result<Vec<Vec<Token>>> {
    Ok(pieces(ddl)?.into_iter().filter(|s| said(s)).collect())
}

/// A DDL's text with its last statement ended, as psql ends a file's last statement: as written
/// where a `;` ends it, a comment after the last `;` being no statement; else with a `;` after it,
/// on a line of its own where a comment to the line's end follows the statement.
pub(crate) fn ended(ddl: &str) -> Result<std::borrow::Cow<'_, str>> {
    let last = pieces(ddl)?.pop().unwrap_or_default();
    if !said(&last) {
        return Ok(std::borrow::Cow::Borrowed(ddl));
    }
    let to_line_end = last
        .iter()
        .rev()
        .find(|t| {
            !matches!(
                t,
                Token::Whitespace(Whitespace::Space | Whitespace::Tab | Whitespace::Newline)
            )
        })
        .is_some_and(|t| matches!(t, Token::Whitespace(Whitespace::SingleLineComment { .. })));
    let end = if to_line_end { "\n;\n" } else { ";\n" };
    Ok(std::borrow::Cow::Owned(format!("{}{end}", ddl.trim_end())))
}

/// Whether a piece of a DDL between its semicolons holds more than whitespace and comments.
fn said(piece: &[Token]) -> bool {
    piece.iter().any(|t| {
        !matches!(
            t,
            Token::Whitespace(
                Whitespace::Space
                    | Whitespace::Tab
                    | Whitespace::Newline
                    | Whitespace::SingleLineComment { .. }
                    | Whitespace::MultiLineComment(_)
            )
        )
    })
}

/// A DDL split on its semicolons, the piece after the last among them: whitespace and comments
/// only where a `;` ends the last statement.
fn pieces(ddl: &str) -> Result<Vec<Vec<Token>>> {
    cut(&crate::read::scanned_unseeded(ddl, crate::regs::LATEST)?)
}

/// A CREATE VIEW's words as `scanned_view` reads its text, where `statements` leaves them as written.
fn view_scanned(tokens: &[Token]) -> Result<Vec<Token>> {
    let text = written(tokens);
    let text = crate::read::scanned_view(&text, crate::regs::LATEST)?;
    Ok(crate::read::image::tokens(&text)?
        .into_iter()
        .map(|t| t.token)
        .collect())
}

/// The statements of a text as written, split on their semicolons as `statements` splits a DDL's.
pub(crate) fn semicolons(ddl: &str) -> Result<Vec<Vec<Token>>> {
    Ok(cut(ddl)?.into_iter().filter(|s| said(s)).collect())
}

/// A text split on its semicolons as it is given, every piece kept, the one after the last `;`
/// among them.
fn cut(ddl: &str) -> Result<Vec<Vec<Token>>> {
    let tokens = crate::read::image::tokens(ddl)?;
    let mut out = vec![Vec::new()];
    // A routine's BEGIN … END keeps the semicolons inside it, as psql keeps them: in a statement
    // that begins CREATE [OR REPLACE] FUNCTION or PROCEDURE, each BEGIN outside parentheses opens
    // a block, a CASE inside one opens another, and END closes the last.
    let (mut words, mut depth, mut parens) = (Vec::<String>::new(), 0usize, 0i64);
    for t in tokens.into_iter().map(|t| t.token) {
        if t == Token::SemiColon && depth == 0 {
            out.push(Vec::new());
            words.clear();
            parens = 0;
            continue;
        }
        match &t {
            Token::LParen => parens += 1,
            Token::RParen => parens -= 1,
            Token::Word(w) if w.quote_style.is_none() => {
                let word = w.value.to_ascii_lowercase();
                if words.len() < 4 {
                    words.push(word.clone());
                }
                if parens == 0 && routine(&words) {
                    match word.as_str() {
                        "begin" => depth += 1,
                        "case" if depth >= 1 => depth += 1,
                        "end" if depth > 0 => depth -= 1,
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        out.last_mut().unwrap().push(t);
    }
    Ok(out)
}

/// A statement as written, without the comments before it.
pub(crate) fn written(tokens: &[Token]) -> String {
    let start = tokens
        .iter()
        .position(|t| !matches!(t, Token::Whitespace(_)))
        .unwrap_or(tokens.len());
    tokens[start..]
        .iter()
        .map(token_written)
        .collect::<String>()
        .trim()
        .to_string()
}

/// A token as it reads again as itself: a quoted string, or a quoted name, its quotes inside it
/// doubled, or escaped in an E'' string, where sqlparser 0.63 prints them bare.
fn token_written(t: &Token) -> String {
    match t {
        Token::SingleQuotedString(s) => format!("'{}'", s.replace('\'', "''")),
        Token::NationalStringLiteral(s) => format!("N'{}'", s.replace('\'', "''")),
        Token::EscapedStringLiteral(s) => {
            format!("E'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
        }
        Token::Word(w) if w.quote_style == Some('"') => {
            format!("\"{}\"", w.value.replace('"', "\"\""))
        }
        other => other.to_string(),
    }
}

/// `ALTER TABLE <table> CLUSTER ON <index>`, which sqlparser does not parse: (table, index).
pub(crate) fn cluster_on(tokens: &[Token]) -> Option<(String, String)> {
    let mut c = Cursor::new(tokens);
    if !c.kws(&["ALTER", "TABLE"]) {
        return None;
    }
    c.kw("ONLY");
    let table = c.name().ok()?;
    if !c.kws(&["CLUSTER", "ON"]) {
        return None;
    }
    let index = c.ident().ok()?;
    c.done().then_some((table, index))
}

/// What a statement's first words say it is, and the name it gives, as best its tokens tell.
pub(crate) fn head(tokens: &[Token]) -> (String, String) {
    let c = Cursor::new(tokens);
    let mut kind = Vec::new();
    let mut i = 0;
    let skip = [
        "OR",
        "REPLACE",
        "MATERIALIZED",
        "UNIQUE",
        "TEMP",
        "TEMPORARY",
        "UNLOGGED",
        "GLOBAL",
        "LOCAL",
    ];
    let objects = [
        "TABLE",
        "INDEX",
        "TYPE",
        "DOMAIN",
        "SEQUENCE",
        "COLLATION",
        "FUNCTION",
        "VIEW",
        "SCHEMA",
        "EXTENSION",
        "TRIGGER",
        "PROCEDURE",
        "AGGREGATE",
        "OPERATOR",
        "POLICY",
        "RULE",
    ];
    while let Some(Token::Word(w)) = c.t.get(i) {
        let v = w.value.to_uppercase();
        if w.quote_style.is_some() {
            break;
        }
        kind.push(v.clone());
        i += 1;
        if objects.contains(&v.as_str()) {
            break;
        }
        if i > 1 && !skip.contains(&v.as_str()) {
            break;
        }
    }
    let mut c = Cursor { t: c.t, i };
    c.kws(&["IF", "NOT", "EXISTS"]);
    c.kws(&["IF", "EXISTS"]);
    c.kw("CONCURRENTLY");
    c.kw("ONLY");
    if kind.last().is_some_and(|k| k == "INDEX") && c.is("ON") {
        // An index the DDL leaves unnamed: named for its table.
        c.kw("ON");
        c.kw("ONLY");
        return (
            kind.join(" "),
            format!("an index on {}", c.name().unwrap_or_default()),
        );
    }
    let object = c.name().unwrap_or_default();
    (kind.join(" "), object)
}

/// The words of a statement, white space and comments left out, unquoted words folded: what
/// sqlparser's own printing of it must give back.
fn words(tokens: &[Token]) -> Vec<String> {
    let mut out: Vec<String> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .map(|t| match t {
            Token::Word(w) if w.quote_style.is_none() => w.value.to_lowercase(),
            Token::Word(w) => format!("\"{}\"", w.value),
            other => other.to_string(),
        })
        .collect();
    out.sort();
    out
}

/// Refuses a statement sqlparser read otherwise than it is written: one whose printing back holds
/// other words than it, so nothing it declares is dropped without a word. The words are compared
/// as a whole, in any order, since sqlparser prints a statement's clauses in an order of its own.
pub(crate) fn read_as_written(tokens: &[Token], st: &Statement, what: &str) -> Result<()> {
    read_as_labelled(tokens, st, what, 0)
}

/// `read_as_written` of a statement writing `bare` labels without AS, which sqlparser prints after
/// one.
fn read_as_labelled(tokens: &[Token], st: &Statement, what: &str, bare: usize) -> Result<()> {
    let back: Vec<Token> = crate::read::image::tokens(&st.to_string())?
        .into_iter()
        .map(|t| t.token)
        .collect();
    let (a, b) = (words(tokens), words(&back));
    if a == b {
        return Ok(());
    }
    // Each word counted as often as it is written.
    let less = |x: &[String], y: &[String]| {
        let mut y = y.to_vec();
        let mut out = Vec::new();
        for w in x {
            match y.iter().position(|v| v == w) {
                Some(i) => {
                    y.remove(i);
                }
                None => out.push(w.clone()),
            }
        }
        out
    };
    let lost = less(&a, &b);
    let mut more = less(&b, &a);
    // A COLLATE after an operand that binds tighter is read with that operand in parentheses
    // (`grammar::POSTGRES`): a pair of them for each such COLLATE is no change.
    let mut wrapped = 0;
    let _ = sqlparser::ast::visit_expressions(st, |e| {
        if matches!(e, Expr::Collate { expr, .. } if matches!(**expr, Expr::Nested(_))) {
            wrapped += 1;
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    for paren in ["(", ")"] {
        for _ in 0..wrapped {
            if let Some(i) = more.iter().position(|w| w == paren) {
                more.remove(i);
            }
        }
    }
    if lost.is_empty() && more.len() == bare && more.iter().all(|w| w == "as") {
        return Ok(());
    }
    Err(Error::new(format!(
        "{what}: sqlparser 0.63 reads it otherwise than it is written ({}{})",
        if lost.is_empty() {
            String::new()
        } else {
            format!(
                "it drops {}",
                lost.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        },
        if more.is_empty() {
            String::new()
        } else {
            format!(
                "{}it adds {}",
                if lost.is_empty() { "" } else { "; " },
                more.iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(" ")
            )
        }
    )))
}

const NAMEDATALEN: usize = 64;

/// The longest prefix of `s` of at most `n` bytes that ends on a character.
fn clip(s: &str, n: usize) -> &str {
    let mut n = n.min(s.len());
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// A name made of two names and a label, as PostgreSQL's makeObjectName makes it: the longer name
/// cut first until the whole fits 63 bytes.
fn object_name_of(name1: &str, name2: Option<&str>, label: &str) -> String {
    let mut overhead = label.len() + 1;
    if name2.is_some() {
        overhead += 1;
    }
    let avail = NAMEDATALEN - 1 - overhead;
    let (mut n1, mut n2) = (name1.len(), name2.map_or(0, str::len));
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let mut out = clip(name1, n1).to_string();
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(clip(n, n2));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// A name no other in the schema holds, as PostgreSQL's ChooseRelationName and
/// ChooseConstraintName choose one: the label, then the label and 1, 2 and on.
fn choose(name1: &str, name2: Option<&str>, label: &str, taken: &dyn Fn(&str) -> bool) -> String {
    let mut pass = 0;
    let mut modlabel = label.to_string();
    loop {
        let n = object_name_of(name1, name2, &modlabel);
        if !taken(&n) {
            return n;
        }
        pass += 1;
        modlabel = format!("{label}{pass}");
    }
}

/// The names of an index's columns joined for its name, as PostgreSQL's ChooseIndexNameAddition
/// joins them.
fn name_addition(names: &[String]) -> String {
    let mut buf = String::new();
    for n in names {
        if !buf.is_empty() {
            buf.push('_');
        }
        let room = NAMEDATALEN - 1 - buf.len();
        buf.push_str(clip(n, room));
        if buf.len() >= NAMEDATALEN - 1 {
            break;
        }
    }
    buf
}

/// The name PostgreSQL gives an index's key in its name: a column's own, a function's, the type of
/// a cast, or "expr"; a name given twice takes a number.
fn key_names(keys: &[Expr]) -> Vec<String> {
    fn figure(e: &Expr) -> Option<String> {
        match e {
            Expr::Identifier(i) => Some(ident(i)),
            Expr::CompoundIdentifier(p) => p.last().map(ident),
            Expr::Nested(x) | Expr::Collate { expr: x, .. } => figure(x),
            Expr::Function(f) => f.name.0.last().map(|p| match p {
                ObjectNamePart::Identifier(i) => ident(i),
                other => other.to_string(),
            }),
            Expr::Extract { .. } => Some("extract".into()),
            Expr::Cast {
                expr, data_type, ..
            } => figure(expr).or_else(|| {
                Some(
                    format_type(data_type)
                        .split(['(', ' '])
                        .next()
                        .unwrap_or("expr")
                        .to_string(),
                )
            }),
            _ => None,
        }
    }
    let mut out: Vec<String> = Vec::new();
    for k in keys {
        let orig = figure(k).unwrap_or_else(|| "expr".into());
        let mut name = orig.clone();
        let mut i = 1;
        while out.contains(&name) {
            let suffix = i.to_string();
            name = format!("{}{suffix}", clip(&orig, NAMEDATALEN - 1 - suffix.len()));
            i += 1;
        }
        out.push(name);
    }
    out
}

fn schema_of(name: &str) -> &str {
    name.rsplit_once('.').map_or("", |(s, _)| s)
}

fn bare(name: &str) -> &str {
    name.rsplit('.').next().unwrap_or(name)
}

/// PostgreSQL's refusal of a type of a name a type stands under.
fn type_taken(name: &str) -> Error {
    Error::new(format!("type \"{}\" already exists", bare(name)))
}

/// PostgreSQL's refusal of a relation whose row type would take the name a type stands under.
fn row_type_taken(name: &str) -> Error {
    Error::new(format!(
        "type \"{}\" already exists\nHINT:  A relation has an associated type of the same name, so \
         you must use a name that doesn't conflict with any existing type.",
        bare(name)
    ))
}

fn qualified(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// An index of a table, read as an order.
#[derive(Clone, Debug)]
struct Keyed {
    name: String,
    method: Method,
    predicate: Option<String>,
    /// The columns its predicate reads, which its strand carries.
    predicate_columns: Vec<usize>,
    exclusion: Option<crate::catalog::Exclusion>,
    levels: Vec<Level>,
    include: Vec<usize>,
    unique: bool,
    nulls_first: Vec<bool>,
    nulls_distinct: bool,
    desc: Vec<bool>,
    keys: Vec<String>,
    /// Each key's operator class's parameters, as written.
    params: Vec<Option<String>>,
    opclasses: Vec<String>,
    options: Vec<(String, String)>,
    constraint: Option<KeyConstraint>,
}

/// A table as the DDL has declared it so far.
#[derive(Clone, Debug)]
struct Declared {
    name: String,
    columns: Vec<Column>,
    primary: Option<Keyed>,
    uniques: Vec<Keyed>,
    checks: Vec<Check>,
    indexes: Vec<Keyed>,
    clustered: Option<String>,
    foreign_keys: Vec<ForeignKey>,
}

impl Declared {
    fn keys(&self) -> impl Iterator<Item = &Keyed> {
        self.primary
            .iter()
            .chain(self.uniques.iter())
            .chain(self.indexes.iter())
    }
}

/// The catalog as the DDL has declared it so far.
#[derive(Clone, Debug, Default)]
struct Draft {
    functions: Vec<Function>,
    views: Vec<View>,
    tables: Vec<Declared>,
    types: Vec<Type>,
    domains: Vec<Domain>,
    sequences: Vec<Sequence>,
    collations: Vec<Collation>,
    /// The CHECKs' and the foreign keys' names, each schema-qualified, which PostgreSQL chooses
    /// unique in a schema beside the keys' names.
    check_names: Vec<String>,
    /// Whether the statement now read is the file's own, which a build without the feature
    /// `collations` reads naming a library collation, refusing that collation only where used.
    kept: bool,
    /// Each index held as element strands, with the number of tables declared before it: its
    /// strands lie among the tables where its statement stands, so that no later statement moves a
    /// table. A DROP that takes a table out moves those after it down.
    strands_at: Vec<(usize, String, String)>,
    /// The schemas CREATE SCHEMA made, in the order made; a schema an object names that no
    /// statement made stands while an object lies in it.
    schemas: Vec<String>,
    /// The release whose operator classes are each type's defaults.
    release: u8,
    /// Each table's place in `tables`, by its name.
    table_at: HashMap<String, usize>,
    /// The relations of each name in each schema, counted: the tables, their keys, the sequences
    /// and the views, as `relation` finds them.
    relations: HashMap<(String, String), usize>,
    /// The primary and unique keys of each name in each schema, counted, as `constraint` finds
    /// them beside `check_names`.
    key_constraints: HashMap<(String, String), usize>,
    /// `check_names`, counted.
    check_named: HashMap<String, usize>,
    /// The OIDs the statements read so far have handed out.
    oids: crate::oids::Oids,
    /// The objects the statement now read renamed, each keeping its OID.
    renamed: Vec<(crate::oids::Object, crate::oids::Object)>,
    /// The descriptions COMMENT ON gave, each object by its OID, so that one dropped takes its own
    /// and one made again under its name has none.
    described: Vec<Description>,
    /// The place of the statement now read among those taken.
    at: usize,
    /// The statements whose OIDs depart from the text's order, as the file's root records them.
    record: Vec<(u32, u32)>,
}

/// What a COMMENT ON names: its object's OID, the catalog that holds it, and its column by name and
/// number.
type Target = (u32, u32, Option<(String, i32)>);

/// A description COMMENT ON gave: the object by its OID and the catalog that holds it, a column by
/// its name, its number read where the description is.
#[derive(Clone, Debug)]
struct Description {
    objoid: u32,
    classoid: u32,
    column: Option<(String, i32)>,
    text: String,
}

/// How many of each kind of object a draft holds, before a statement is read.
#[derive(Clone, Copy)]
struct Lens {
    tables: usize,
    types: usize,
    domains: usize,
    sequences: usize,
    collations: usize,
    views: usize,
    functions: usize,
    schemas: usize,
}

/// One more of a name.
fn count_in<K: std::hash::Hash + Eq>(names: &mut HashMap<K, usize>, name: K) {
    *names.entry(name).or_default() += 1;
}

/// One fewer of a name.
fn count_out<K: std::hash::Hash + Eq>(names: &mut HashMap<K, usize>, name: &K) {
    if let Some(n) = names.get_mut(name) {
        *n -= 1;
        if *n == 0 {
            names.remove(name);
        }
    }
}

/// A name in its schema, as `relation` and `constraint` compare it: one written with no schema is
/// public's, as `public.x` is.
fn in_schema(schema: &str, name: &str) -> (String, String) {
    let schema = if schema.is_empty() { "public" } else { schema };
    (schema.to_string(), name.to_string())
}

/// A key a CREATE TABLE writes, before its name is chosen.
struct KeySpec {
    name: Option<String>,
    primary: bool,
    columns: Vec<String>,
    include: Vec<String>,
    nulls_distinct: bool,
    deferrable: bool,
    /// Its last column WITHOUT OVERLAPS.
    without_overlaps: bool,
}

impl Draft {
    /// The tables and views made so far, as a function body's subqueries read them.
    fn relations_read(&self) -> crate::read::own::Relations {
        use crate::read::own::Readable;
        let tables = self.tables.iter().map(|t| {
            let columns = t.columns.iter().map(|c| c.name.clone()).collect();
            (crate::oids::full(&t.name), Readable { columns, table: true })
        });
        let views = self.views.iter().map(|v| {
            let columns = v.columns.clone();
            (crate::oids::full(&v.name), Readable { columns, table: false })
        });
        tables.chain(views).collect()
    }

    fn table(&self, name: &str) -> Option<&Declared> {
        let found = self.table_at.get(name).map(|&i| &self.tables[i]);
        debug_assert_eq!(
            found.map(|t| &t.name),
            self.tables.iter().find(|t| t.name == name).map(|t| &t.name)
        );
        found
    }

    fn table_mut(&mut self, name: &str) -> Option<&mut Declared> {
        let i = *self.table_at.get(name)?;
        Some(&mut self.tables[i])
    }

    /// A table declared, its name and its keys' names counted.
    fn push_table(&mut self, table: Declared) {
        self.table_at
            .entry(table.name.clone())
            .or_insert(self.tables.len());
        let schema = schema_of(&table.name);
        count_in(&mut self.relations, in_schema(schema, bare(&table.name)));
        for k in table.keys() {
            count_in(&mut self.relations, in_schema(schema, &k.name));
        }
        for k in table.primary.iter().chain(table.uniques.iter()) {
            count_in(&mut self.key_constraints, in_schema(schema, &k.name));
        }
        self.tables.push(table);
    }

    /// The tables, sequences and constraints' names past the counts given taken back, as a
    /// statement refused leaves them.
    fn take_back(&mut self, tables: usize, sequences: usize, check_names: usize) {
        for t in self.tables.drain(tables..) {
            let schema = schema_of(&t.name);
            count_out(&mut self.relations, &in_schema(schema, bare(&t.name)));
            for k in t.keys() {
                count_out(&mut self.relations, &in_schema(schema, &k.name));
            }
            for k in t.primary.iter().chain(t.uniques.iter()) {
                count_out(&mut self.key_constraints, &in_schema(schema, &k.name));
            }
        }
        self.table_at.retain(|_, &mut i| i < tables);
        for s in self.sequences.drain(sequences..) {
            count_out(
                &mut self.relations,
                &in_schema(schema_of(&s.name), bare(&s.name)),
            );
        }
        for q in self.check_names.drain(check_names..) {
            count_out(&mut self.check_named, &q);
        }
    }

    /// A sequence declared, its name counted.
    fn push_sequence(&mut self, sequence: Sequence) {
        count_in(
            &mut self.relations,
            in_schema(schema_of(&sequence.name), bare(&sequence.name)),
        );
        self.sequences.push(sequence);
    }

    /// A CHECK's or a foreign key's name, schema-qualified, taken and counted.
    fn push_check_name(&mut self, q: String) {
        count_in(&mut self.check_named, q.clone());
        self.check_names.push(q);
    }

    /// Whether a relation of that name lies in the schema: a table, an index or a sequence.
    fn relation(&self, schema: &str, name: &str) -> bool {
        let found = self.relations.contains_key(&in_schema(schema, name));
        debug_assert_eq!(found, self.relation_walked(schema, name), "{schema}.{name}");
        found
    }

    /// `relation`, every table, sequence and view walked.
    fn relation_walked(&self, schema: &str, name: &str) -> bool {
        let schema = in_schema(schema, "").0;
        let of = |n: &str| in_schema(schema_of(n), "").0;
        self.tables.iter().any(|t| {
            (of(&t.name) == schema && bare(&t.name) == name)
                || (of(&t.name) == schema && t.keys().any(|k| k.name == name))
        }) || self
            .sequences
            .iter()
            .any(|s| of(&s.name) == schema && bare(&s.name) == name)
            || self
                .views
                .iter()
                .any(|v| of(&v.name) == schema && bare(&v.name) == name)
    }

    /// Whether a constraint of that name lies in the schema: a key's or a CHECK's.
    fn constraint(&self, schema: &str, name: &str) -> bool {
        let found = self.check_named.contains_key(&qualified(schema, name))
            || self.key_constraints.contains_key(&in_schema(schema, name));
        debug_assert_eq!(found, self.constraint_walked(schema, name), "{schema}.{name}");
        found
    }

    /// `constraint`, every name and table walked.
    fn constraint_walked(&self, schema: &str, name: &str) -> bool {
        self.check_names.contains(&qualified(schema, name))
            || self.tables.iter().any(|t| {
                in_schema(schema_of(&t.name), "") == in_schema(schema, "")
                    && t.primary
                        .iter()
                        .chain(t.uniques.iter())
                        .any(|k| k.name == name)
            })
    }

    fn lens(&self) -> Lens {
        Lens {
            tables: self.tables.len(),
            types: self.types.len(),
            domains: self.domains.len(),
            sequences: self.sequences.len(),
            collations: self.collations.len(),
            views: self.views.len(),
            functions: self.functions.len(),
            schemas: self.schemas.len(),
        }
    }

    /// The objects a table makes, in the order it makes them: the relation, its row type and the
    /// type's array, each DEFAULT, each NOT NULL, each key and its index, each CHECK, each foreign
    /// key, then each index and the EXCLUDE it stands for; each after a constraint its triggers.
    fn table_objects(t: &Declared, out: &mut Vec<crate::oids::Object>) {
        use crate::oids::{trigger, Object as O};
        let name = crate::oids::full(&t.name);
        let (schema, own) = crate::oids::parts(&name);
        let (schema, own) = (schema.to_string(), own.to_string());
        let deferrable = |k: &Keyed| {
            k.constraint.as_ref().is_some_and(|c| c.deferrable)
                || k.exclusion.as_ref().is_some_and(|x| x.deferrable)
        };
        out.push(O::Class(name.clone()));
        out.push(O::Type(name.clone()));
        out.push(O::Array(name.clone()));
        for c in t.columns.iter().filter(|c| c.default.is_some() || c.generated.is_some()) {
            out.push(O::Default(name.clone(), c.name.clone()));
        }
        for c in t.columns.iter().filter(|c| c.not_null) {
            out.push(O::Constraint(
                name.clone(),
                format!("{own}_{}_not_null", c.name),
            ));
        }
        for k in t.primary.iter().chain(&t.uniques) {
            out.push(O::Constraint(name.clone(), k.name.clone()));
            out.push(O::Class(format!("{schema}.{}", k.name)));
            if deferrable(k) {
                out.push(O::Trigger(name.clone(), k.name.clone(), trigger::RECHECK));
            }
        }
        for c in &t.checks {
            out.push(O::Constraint(name.clone(), c.name.clone()));
        }
        for f in &t.foreign_keys {
            out.push(O::Constraint(name.clone(), f.name.clone()));
            if f.enforced {
                for place in [
                    trigger::ACTION_DELETE,
                    trigger::ACTION_UPDATE,
                    trigger::CHECK_INSERT,
                    trigger::CHECK_UPDATE,
                ] {
                    out.push(O::Trigger(name.clone(), f.name.clone(), place));
                }
            }
        }
        for k in &t.indexes {
            if k.exclusion.is_some() {
                out.push(O::Constraint(name.clone(), k.name.clone()));
            }
            out.push(O::Class(format!("{schema}.{}", k.name)));
            if k.exclusion.is_some() && deferrable(k) {
                out.push(O::Trigger(name.clone(), k.name.clone(), trigger::RECHECK));
            }
        }
    }

    /// The objects a type makes: the type and its array, an enum's labels, a composite's relation,
    /// a range's multirange and its array, the functions that construct each, and the range's cast
    /// to its multirange.
    fn type_objects(t: &Type, out: &mut Vec<crate::oids::Object>) {
        use crate::oids::Object as O;
        let name = crate::oids::full(&t.name);
        out.push(O::Type(name.clone()));
        out.push(O::Array(name.clone()));
        match &t.kind {
            TypeKind::Enum { labels } => {
                for l in labels {
                    out.push(O::Label(name.clone(), l.clone()));
                }
            }
            TypeKind::Composite { .. } => out.push(O::Class(name.clone())),
            TypeKind::Range(r) => {
                let m = crate::oids::full(&r.multirange);
                out.push(O::Type(m.clone()));
                out.push(O::Array(m.clone()));
                let sub = crate::regs::type_oid(&r.subtype, crate::regs::LATEST)
                    .map(|o| crate::regs::reg_text(crate::regs::RegKind::Type, o, crate::regs::LATEST))
                    .unwrap_or_else(|_| r.subtype.clone());
                out.push(O::Proc(name.clone(), format!("{sub},{sub}")));
                out.push(O::Proc(name.clone(), format!("{sub},{sub},text")));
                let shown = match crate::oids::parts(&name) {
                    ("public", n) => n.to_string(),
                    _ => name.clone(),
                };
                out.push(O::Proc(m.clone(), String::new()));
                out.push(O::Proc(m.clone(), shown.clone()));
                out.push(O::Proc(m, format!("{shown}[]")));
                out.push(O::Cast(name.clone()));
            }
        }
    }

    fn domain_objects(d: &Domain, out: &mut Vec<crate::oids::Object>) {
        use crate::oids::Object as O;
        let name = crate::oids::full(&d.name);
        out.push(O::Type(name.clone()));
        out.push(O::Array(name.clone()));
        if d.not_null {
            let own = crate::oids::parts(&name).1;
            let n = d
                .not_null_name
                .clone()
                .unwrap_or_else(|| format!("{own}_not_null"));
            out.push(O::Constraint(name.clone(), n));
        }
        for k in &d.checks {
            out.push(O::Constraint(name.clone(), k.name.clone()));
        }
    }

    fn view_objects(v: &View, out: &mut Vec<crate::oids::Object>) {
        use crate::oids::Object as O;
        let name = crate::oids::full(&v.name);
        out.push(O::Class(name.clone()));
        out.push(O::Type(name.clone()));
        out.push(O::Array(name.clone()));
        out.push(O::Rule(name));
    }

    fn function_object(f: &Function) -> crate::oids::Object {
        let args: Vec<String> = f.args.iter().map(|t| t.pg_name()).collect();
        crate::oids::Object::Proc(crate::oids::full(&f.name), args.join(","))
    }

    /// Every object the draft holds that PostgreSQL gives an OID, each schema before its first.
    fn objects(&self) -> Vec<crate::oids::Object> {
        let all = Lens {
            tables: 0,
            types: 0,
            domains: 0,
            sequences: 0,
            collations: 0,
            views: 0,
            functions: 0,
            schemas: 0,
        };
        self.objects_since(all, &[])
    }

    /// The objects past `before` of each kind, and those of the tables, types and domains named, each
    /// schema before its first.
    fn objects_since(&self, before: Lens, touched: &[&str]) -> Vec<crate::oids::Object> {
        use crate::oids::Object as O;
        let mut out = Vec::new();
        for s in &self.schemas[before.schemas.min(self.schemas.len())..] {
            out.push(O::Namespace(s.clone()));
        }
        for c in &self.collations[before.collations.min(self.collations.len())..] {
            out.push(O::Collation(crate::oids::full(&c.name)));
        }
        let types = self.types.iter().enumerate().filter(|(i, t)| {
            *i >= before.types || touched.contains(&t.name.as_str())
        });
        for (_, t) in types {
            Self::type_objects(t, &mut out);
        }
        let domains = self.domains.iter().enumerate().filter(|(i, d)| {
            *i >= before.domains || touched.contains(&d.name.as_str())
        });
        for (_, d) in domains {
            Self::domain_objects(d, &mut out);
        }
        for s in &self.sequences[before.sequences.min(self.sequences.len())..] {
            out.push(O::Class(crate::oids::full(&s.name)));
        }
        for t in &self.tables[before.tables.min(self.tables.len())..] {
            Self::table_objects(t, &mut out);
        }
        for name in touched {
            if let Some(t) = self.table(name) {
                Self::table_objects(t, &mut out);
            }
        }
        for v in &self.views[before.views.min(self.views.len())..] {
            Self::view_objects(v, &mut out);
        }
        for f in &self.functions[before.functions.min(self.functions.len())..] {
            out.push(Self::function_object(f));
        }
        let mut seen: Vec<String> = Vec::new();
        let mut with_schemas = Vec::with_capacity(out.len());
        for o in out {
            if let O::Namespace(n) = &o {
                if !seen.contains(n) {
                    seen.push(n.clone());
                    with_schemas.push(o);
                }
                continue;
            }
            let schema = crate::oids::parts(o.name()).0;
            if schema != "public" && schema != "pg_catalog" && !seen.iter().any(|s| s == schema) {
                seen.push(schema.to_string());
                with_schemas.push(O::Namespace(schema.to_string()));
            }
            with_schemas.push(o);
        }
        with_schemas
    }

    /// The OIDs a statement just read hands out: to each object it made or touched, from where the
    /// text goes on, or from the first the file's record gives the statement. A statement that drops
    /// or renames settles every object, those no longer standing giving their OIDs up.
    fn settle(&mut self, before: Lens, kind: &str, object: &str, tokens: &[Token]) {
        let recorded = self
            .record
            .binary_search_by_key(&(self.at as u32), |r| r.0)
            .ok()
            .map(|i| self.record[i].1);
        let begun = self.oids.begin(recorded);
        self.settled(before, kind, object, tokens);
        self.oids.end(self.at, begun, recorded.is_some());
    }

    fn settled(&mut self, before: Lens, kind: &str, object: &str, tokens: &[Token]) {
        let renamed = std::mem::take(&mut self.renamed);
        let ws = words(tokens);
        let wide = !renamed.is_empty()
            || kind.starts_with("DROP")
            || (kind.starts_with("ALTER") && ws.iter().any(|w| w == "drop" || w == "rename"));
        if wide {
            let all = self.objects();
            self.oids.settle(&all, &renamed);
        } else {
            let on = if kind.contains("INDEX") {
                index_on(&written(tokens))
            } else {
                None
            };
            let mut touched: Vec<&str> = Vec::new();
            if kind.starts_with("ALTER") {
                touched.push(object);
            }
            if let Some(t) = on.as_deref() {
                touched.push(t);
            }
            let made = self.objects_since(before, &touched);
            self.oids.give(&made);
        }
        self.publish_descriptions();
    }

    /// The object a COMMENT ON names: its OID, the catalog that holds it and its column, found as
    /// PostgreSQL's search path finds a name with no schema, refused in PostgreSQL's words where
    /// it names none or one of another kind; PostgreSQL's own objects refused by name.
    fn commented(&self, target: &Commented) -> Result<Target> {
        use crate::deparse::depend as d;
        use crate::oids::{full, parts, Object as O};
        fn own_refused<T>(what: &str) -> Result<T> {
            Err(Error::new(format!(
                "COMMENT ON {what} is refused: PostgreSQL's own objects are the release's, which \
                 a snail carries beside its file, not objects its DDL made"
            )))
        }
        let catalog_schema =
            |s: &str| matches!(s, "pg_catalog" | "information_schema" | "pg_toast");
        // A relation by its schema and its own name: its OID and kind.
        let relation = |word: &str, schema: Option<&str>, own: &str, shown: &str| -> Result<(u32, char)> {
            if let Some(s) = schema.filter(|s| catalog_schema(s)) {
                return own_refused(&format!("{word} {s}.{own}"));
            }
            if schema.is_none() && self.built_in_relation(own).is_some() {
                return own_refused(&format!("{word} {own}"));
            }
            let s = schema.unwrap_or("public");
            if schema.is_some() && !self.schema_stands(s) {
                return Err(Error::new(format!("schema \"{s}\" does not exist")));
            }
            let kind = self.relation_kind(s, own).ok_or_else(|| {
                Error::new(format!("relation \"{shown}\" does not exist"))
            })?;
            let oid = self.oids.of(&O::Class(format!("{s}.{own}"))).unwrap_or(0);
            Ok((oid, kind))
        };
        // A type of the file's by its name: PostgreSQL's own refused, one of no name refused.
        let type_oid = |name: &str| -> Result<u32> {
            let f = full(name);
            let (s, _) = parts(&f);
            if catalog_schema(s) || (!name.contains('.') && self.built_in_type(name)) {
                return own_refused(&format!("TYPE {name}"));
            }
            self.oids
                .of(&O::Type(full(name)))
                .ok_or_else(|| Error::new(format!("type \"{name}\" does not exist")))
        };
        let shown_type = |name: &str| match parts(&full(name)) {
            ("public", own) => own.to_string(),
            _ => full(name),
        };
        Ok(match target {
            Commented::Refused(why) => return Err(Error::new(why.clone())),
            Commented::Relation { word, schema, own } => {
                let shown = match schema {
                    Some(s) => format!("{s}.{own}"),
                    None => own.clone(),
                };
                let (oid, kind) = relation(word, schema.as_deref(), own, &shown)?;
                let (wanted, one) = match *word {
                    "TABLE" => ('r', "a table"),
                    "VIEW" => ('v', "a view"),
                    "INDEX" => ('i', "an index"),
                    _ => ('S', "a sequence"),
                };
                if kind != wanted {
                    return Err(Error::new(format!("\"{own}\" is not {one}")));
                }
                (oid, d::CLASS, None)
            }
            Commented::Column(names) => {
                let (schema, own, column) = match names.as_slice() {
                    [_] => return Err(Error::new("column name must be qualified")),
                    [r, c] => (None, r, c),
                    [s, r, c] => (Some(s.as_str()), r, c),
                    _ => {
                        return Err(Error::new(format!(
                            "cross-database references are not implemented: {}",
                            names.join(".")
                        )))
                    }
                };
                let shown = match schema {
                    Some(s) => format!("{s}.{own}"),
                    None => own.clone(),
                };
                let (oid, kind) = relation("COLUMN", schema, own, &shown)?;
                let rel = format!("{}.{own}", schema.unwrap_or("public"));
                let number = match kind {
                    'v' => self.view_column_number(&rel, column)?,
                    _ => self.column_number(&rel, column),
                };
                let number = number.ok_or_else(|| {
                    Error::new(format!(
                        "column \"{column}\" of relation \"{shown}\" does not exist"
                    ))
                })?;
                (oid, d::CLASS, Some((column.clone(), number)))
            }
            Commented::Type { name, domain } => {
                let oid = type_oid(name)?;
                let is_domain = self.domains.iter().any(|x| full(&x.name) == full(name));
                if *domain && !is_domain {
                    return Err(Error::new(format!("\"{}\" is not a domain", shown_type(name))));
                }
                (oid, d::TYPE, None)
            }
            Commented::Function { name, args } => {
                let f = full(name);
                let (s, own) = parts(&f);
                let bare = !name.contains('.');
                if catalog_schema(s) {
                    return own_refused(&format!("FUNCTION {name}"));
                }
                let oid = match args {
                    Some(args) => {
                        let mut texts = Vec::new();
                        for a in args {
                            let ty = crate::catalog::held_type(a).map_err(|e| {
                                Error::new(format!("COMMENT ON FUNCTION {name}: {e}"))
                            })?;
                            texts.push(ty.pg_name());
                        }
                        let joined = texts.join(",");
                        // pg_catalog's of that signature, which the search path finds first.
                        let built_in = crate::regs::reg_parse(
                            crate::regs::RegKind::Procedure,
                            &format!("pg_catalog.{own}({joined})"),
                            self.release,
                        );
                        if bare && built_in.is_ok() {
                            return own_refused(&format!("FUNCTION {name}({joined})"));
                        }
                        self.oids.of(&O::Proc(full(name), joined)).ok_or_else(|| {
                            Error::new(format!(
                                "function {name}({}) does not exist",
                                texts.join(", ")
                            ))
                        })?
                    }
                    None => {
                        let named: Vec<u32> = self
                            .oids
                            .each()
                            .filter(|(_, o)| matches!(o, O::Proc(n, _) if *n == full(name)))
                            .map(|(oid, _)| oid)
                            .collect();
                        let built_in = if bare { self.built_in_functions(own) } else { 0 };
                        match (named.as_slice(), built_in) {
                            ([oid], 0) => *oid,
                            ([], 0) => {
                                return Err(Error::new(format!(
                                    "could not find a function named \"{name}\""
                                )))
                            }
                            ([], 1) => return own_refused(&format!("FUNCTION {name}")),
                            _ => {
                                return Err(Error::new(format!(
                                    "function name \"{name}\" is not unique\nHINT:  Specify the \
                                     argument list to select the function unambiguously."
                                )))
                            }
                        }
                    }
                };
                (oid, d::PROC, None)
            }
            Commented::Constraint { name, on, domain: true } => {
                type_oid(on)?;
                if !self.domains.iter().any(|x| full(&x.name) == full(on)) {
                    return Err(Error::new(format!("\"{}\" is not a domain", shown_type(on))));
                }
                let k = self.oids.of(&O::Constraint(full(on), name.clone())).ok_or_else(|| {
                    Error::new(format!(
                        "constraint \"{name}\" for domain {} does not exist",
                        shown_type(on)
                    ))
                })?;
                (k, d::CONSTRAINT, None)
            }
            Commented::Constraint { name, on, domain: false } => {
                let f = full(on);
                let (s, own) = parts(&f);
                let schema = on.contains('.').then_some(s);
                relation("CONSTRAINT ON", schema, own, on)?;
                let k = self
                    .oids
                    .of(&O::Constraint(full(on), name.clone()))
                    .ok_or_else(|| {
                        Error::new(format!(
                            "constraint \"{name}\" for table \"{own}\" does not exist"
                        ))
                    })?;
                (k, d::CONSTRAINT, None)
            }
            Commented::OnRelation { word, name, on } => {
                let f = full(on);
                let (s, own) = parts(&f);
                let schema = on.contains('.').then_some(s);
                relation(word, schema, own, on)?;
                match *word {
                    "RULE" => {
                        let rule = (name == "_RETURN")
                            .then(|| self.oids.of(&O::Rule(full(on))))
                            .flatten()
                            .ok_or_else(|| {
                                Error::new(format!(
                                    "rule \"{name}\" for relation \"{own}\" does not exist"
                                ))
                            })?;
                        (rule, d::REWRITE, None)
                    }
                    _ => {
                        // A trigger is named by its own OID, after its kind's prefix.
                        let digits = name.trim_start_matches(|c: char| !c.is_ascii_digit());
                        let found = digits.parse::<u32>().ok().filter(|oid| {
                            matches!(self.oids.object(*oid),
                                Some(O::Trigger(t, _, _)) if *t == full(on))
                        });
                        let oid = found.ok_or_else(|| {
                            Error::new(format!(
                                "trigger \"{name}\" for table \"{own}\" does not exist"
                            ))
                        })?;
                        (oid, d::TRIGGER, None)
                    }
                }
            }
            Commented::Schema(name) => {
                if catalog_schema(name) {
                    return own_refused(&format!("SCHEMA {name}"));
                }
                let oid = match self.oids.of(&O::Namespace(name.clone())) {
                    Some(oid) => oid,
                    None if name == "public" && !self.oids.public_dropped() => crate::oids::PUBLIC,
                    None => return Err(Error::new(format!("schema \"{name}\" does not exist"))),
                };
                (oid, d::NAMESPACE, None)
            }
            Commented::Collation(name) => {
                let f = full(name);
                let (s, _) = parts(&f);
                if catalog_schema(s)
                    || (!name.contains('.')
                        && crate::pgcatalog::is_built_in_collation(name, self.release))
                {
                    return own_refused(&format!("COLLATION {name}"));
                }
                let oid = self.oids.of(&O::Collation(full(name))).ok_or_else(|| {
                    Error::new(format!(
                        "collation \"{name}\" for encoding \"UTF8\" does not exist"
                    ))
                })?;
                (oid, d::COLLATION, None)
            }
        })
    }

    /// Whether a type's name with no schema names one of PostgreSQL's, which the search path finds
    /// first.
    fn built_in_type(&self, name: &str) -> bool {
        crate::regs::type_oid(name, self.release).is_ok_and(|oid| oid < crate::oids::FIRST)
    }

    /// How many of PostgreSQL's functions bear a name, which the search path finds first.
    fn built_in_functions(&self, name: &str) -> usize {
        self.built_in_function_oids(name).len()
    }

    /// The OIDs of PostgreSQL's functions that bear a name.
    fn built_in_function_oids(&self, name: &str) -> Vec<u32> {
        crate::pgcatalog::built_in("pg_proc", self.release)
            .iter()
            .filter(|r| {
                r.get("proname").cloned().flatten().as_deref() == Some(name)
                    && r.get("pronamespace").cloned().flatten().as_deref() == Some("11")
            })
            .filter_map(|r| r.get("oid").cloned().flatten()?.parse().ok())
            .collect()
    }

    /// The descriptions as pg_description holds them once a statement is read: those of objects
    /// no longer standing, or of columns gone, given up; each column's by its number now.
    fn publish_descriptions(&mut self) {
        use crate::oids::Object as O;
        let mut rows = Vec::new();
        let mut kept = Vec::new();
        for d in std::mem::take(&mut self.described) {
            let standing = match self.oids.object(d.objoid) {
                Some(_) => true,
                None => d.objoid == crate::oids::PUBLIC && !self.oids.public_dropped(),
            };
            let objsubid = match (&d.column, self.oids.object(d.objoid)) {
                (None, _) => Some(0),
                (Some((c, n)), Some(O::Class(rel))) => match self.tables.iter().any(|t| crate::oids::full(&t.name) == *rel) {
                    true => self.column_number(rel, c),
                    false => Some(*n),
                },
                _ => None,
            };
            let (true, Some(objsubid)) = (standing, objsubid) else {
                continue;
            };
            rows.push(crate::oids::Described {
                objoid: d.objoid,
                classoid: d.classoid,
                objsubid,
                text: d.text.clone(),
            });
            kept.push(d);
        }
        rows.sort_by_key(|r| (r.objoid, r.classoid, r.objsubid));
        self.described = kept;
        self.oids.describe(rows);
    }

    /// A view's column's number, from 1, its names those its question gives under its column list.
    fn view_column_number(&self, relation: &str, column: &str) -> Result<Option<i32>> {
        let Some(v) = self.views.iter().find(|v| crate::oids::full(&v.name) == relation) else {
            return Ok(None);
        };
        let bound = crate::grammar::bound(&v.query);
        let parsed = crate::read::image::parse_sql(bound.as_deref().unwrap_or(&v.query))?;
        let [Statement::Query(q)] = parsed.as_slice() else {
            return Ok(None);
        };
        let mut names = crate::read::view_columns(&self.so_far(), q)?;
        for (n, c) in names.iter_mut().zip(&v.columns) {
            *n = c.clone();
        }
        Ok(names.iter().position(|n| n == column).map(|i| i as i32 + 1))
    }

    /// A column's number in its relation, from 1, where the relation holds it: a table's, a
    /// composite's field or a sequence's.
    fn column_number(&self, relation: &str, column: &str) -> Option<i32> {
        use crate::oids::full;
        let at = |names: Vec<&str>| names.iter().position(|n| *n == column).map(|i| i as i32 + 1);
        if let Some(t) = self.tables.iter().find(|t| full(&t.name) == relation) {
            return at(t.columns.iter().map(|c| c.name.as_str()).collect());
        }
        if self.sequences.iter().any(|s| full(&s.name) == relation) {
            return at(vec!["last_value", "log_cnt", "is_called"]);
        }
        self.types.iter().find(|t| full(&t.name) == relation).and_then(|t| match &t.kind {
            TypeKind::Composite { fields } => at(fields.iter().map(|f| f.0.as_str()).collect()),
            _ => None,
        })
    }

    /// Whether a type of the name stands: a type the DDL declares, a range's multirange, a domain,
    /// or the row type of a table or a view.
    fn type_named(&self, name: &str) -> bool {
        self.types.iter().any(|t| {
            t.name == name || matches!(&t.kind, TypeKind::Range(r) if r.multirange == name)
        }) || self.domains.iter().any(|d| d.name == name)
            || self.table(name).is_some()
            || self.views.iter().any(|v| v.name == name)
    }

    /// Whether a composite type of the name stands, which PostgreSQL keeps as a relation too.
    fn composite_named(&self, name: &str) -> bool {
        self.types
            .iter()
            .any(|t| t.name == name && matches!(t.kind, TypeKind::Composite { .. }))
    }

    /// A type as the DDL names it, and the domain it names where it names one, whose base type it
    /// is held as: a type built in, an array of any type held, an enum, a composite or a range the
    /// DDL declares, a range's multirange, or a table's row type.
    fn held(&self, dt: &DataType, what: &str) -> Result<(ColumnType, Option<String>)> {
        use crate::composed::{type_of, Composed, Field};
        let refused = |why: String| Error::new(format!("{what}: {why}"));
        if let DataType::Array(def) = dt {
            let inner = match def {
                sqlparser::ast::ArrayElemTypeDef::SquareBracket(t, _)
                | sqlparser::ast::ArrayElemTypeDef::AngleBracket(t)
                | sqlparser::ast::ArrayElemTypeDef::Parenthesis(t)
                | sqlparser::ast::ArrayElemTypeDef::Qualified(t, _) => t,
                sqlparser::ast::ArrayElemTypeDef::None => {
                    return Err(refused("an array names its element's type".into()))
                }
            };
            let (element, domain) = self.held(inner, what)?;
            // PostgreSQL holds an array of any number of dimensions as one type.
            if let ColumnType::Composed(r) = element {
                if domain.is_none() && matches!(r.def(), Composed::Array { .. }) {
                    return Ok((element, None));
                }
            }
            return Ok((type_of(Composed::Array { element, domain }), None));
        }
        if let DataType::Custom(name, args) = dt {
            if args.is_empty() {
                let n = object_name(name);
                let n = n.strip_prefix("pg_catalog.").unwrap_or(&n).to_string();
                // As PostgreSQL searches pg_catalog first.
                if let Some(t) = crate::rowtypes::row_type(&n)
                    .map_err(|e| Error::new(format!("{what}: {e}")))?
                {
                    return Ok((t, None));
                }
                if let Some(t) = self.types.iter().find(|t| t.name == n) {
                    return Ok((self.declared_type(t, what)?, None));
                }
                if let Some(t) = self.types.iter().find(|t| match &t.kind {
                    TypeKind::Range(r) => r.multirange == n,
                    _ => false,
                }) {
                    let range = self.declared_type(t, what)?;
                    return Ok((type_of(Composed::Multirange { name: n, range }), None));
                }
                if let Some(t) = crate::composed::built_in(&n) {
                    return Ok((t, None));
                }
                if let Some(d) = self.domains.iter().find(|d| d.name == n) {
                    let (base, _) = self.held(&parse_type(&d.base)?, what)?;
                    return Ok((base, Some(d.name.clone())));
                }
                if let Some(t) = self.tables.iter().find(|t| t.name == n) {
                    let fields = t
                        .columns
                        .iter()
                        .map(|c| Field {
                            name: c.name.clone(),
                            ty: c.ty,
                            domain: c.domain.clone(),
                            collation: None,
                        })
                        .collect();
                    return Ok((type_of(Composed::Record { name: n, fields }), None));
                }
                if let Some(e) = n.strip_prefix('_') {
                    if let Ok(inner) = parse_type(e) {
                        if let Ok((element, domain)) = self.held(&inner, what) {
                            return Ok((type_of(Composed::Array { element, domain }), None));
                        }
                    }
                }
                // A type named in a schema of the file's that declares none of that name, as
                // PostgreSQL's lookup refuses it: the schema first.
                if let Some((schema, _)) = n.rsplit_once('.') {
                    if !self.kept && !self.schema_stands(schema) {
                        return Err(Error::new(format!("schema \"{schema}\" does not exist")));
                    }
                    return Err(Error::new(format!("type \"{n}\" does not exist")));
                }
            }
        }
        held_type(dt)
            .map(|t| (t, None))
            .map_err(|e| Error::new(format!("{what}: {e}")))
    }

    /// A type the DDL declares, as the snail holds it.
    fn declared_type(&self, t: &Type, what: &str) -> Result<ColumnType> {
        use crate::composed::{type_of, Composed, Field};
        let refused = |why: String| Error::new(format!("{what}: {}, {why}", t.name));
        Ok(match &t.kind {
            TypeKind::Enum { labels } => type_of(Composed::Enum {
                name: t.name.clone(),
                labels: labels.clone(),
            }),
            TypeKind::Composite { fields } => {
                let fields = fields
                    .iter()
                    .map(|(name, ty, collation)| {
                        let casing = match collation {
                            Some(k) => match crate::catalog::collation_held(k, &self.collations) {
                                Some(c) => Some(c),
                                None => {
                                    return Err(refused(format!(
                                        "its field {name} names COLLATE {k}: text is compared by its bytes"
                                    )))
                                }
                            },
                            None => None,
                        };
                        let (ty, domain) = self.held(&parse_type(ty)?, what)?;
                        Ok(Field {
                            name: name.clone(),
                            ty,
                            domain,
                            collation: match casing {
                                Some(crate::catalog::Casing::Library(id)) => Some(id),
                                _ => None,
                            },
                        })
                    })
                    .collect::<Result<Vec<_>>>()?;
                type_of(Composed::Record {
                    name: t.name.clone(),
                    fields,
                })
            }
            TypeKind::Range(r) => {
                if let Some(c) = &r.canonical {
                    return Err(refused(format!(
                        "a range whose CANONICAL is {c}, a function the snail does not run"
                    )));
                }
                if let Some(k) = &r.collation {
                    if crate::catalog::collation_held(k, &self.collations).is_none() {
                        return Err(refused(format!(
                            "a range under COLLATE {k}: text is compared by its bytes"
                        )));
                    }
                }
                let (subtype, domain) = self.held(&parse_type(&r.subtype)?, what)?;
                if domain.is_some() || subtype.is_nested() || subtype.is_bits() {
                    return Err(refused(format!(
                        "a range of {}, which is not held",
                        r.subtype
                    )));
                }
                if let Some(o) = &r.opclass {
                    if *o != subtype.default_opclass() {
                        return Err(refused(format!(
                            "a range on the operator class {o}, where only {}'s default is read",
                            r.subtype
                        )));
                    }
                }
                if !subtype.ordered() {
                    return Err(refused(format!(
                        "a range of {}, which has no B-tree",
                        r.subtype
                    )));
                }
                type_of(Composed::Range {
                    name: t.name.clone(),
                    subtype,
                    discrete: false,
                })
            }
        })
    }

    /// A sequence named as nextval's argument names it, `'s.seq'` or `'s.seq'::regclass`.
    fn nextval_of(&self, e: &Expr) -> Result<String> {
        let text = match e {
            Expr::Value(v) => match &v.value {
                Value::SingleQuotedString(s) => s.clone(),
                _ => return Err(Error::new(format!("nextval({e}) names no sequence"))),
            },
            Expr::Cast { expr, .. } | Expr::Nested(expr) => return self.nextval_of(expr),
            _ => return Err(Error::new(format!("nextval({e}) names no sequence"))),
        };
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text).tokenize()?;
        let mut c = Cursor::new(&tokens);
        let name = c
            .name()
            .map_err(|_| Error::new(format!("nextval('{text}') names no sequence")))?;
        if !c.done() {
            return Err(Error::new(format!("nextval('{text}') names no sequence")));
        }
        if !self.sequences.iter().any(|s| s.name == name) {
            return Err(Error::new(format!("relation \"{name}\" does not exist")));
        }
        Ok(name)
    }

    /// Refuses a DEFAULT the snail cannot fill a column of type `ty` with: one that reads a column,
    /// calls what it does not read, or hands out a value PostgreSQL would not assign to the column.
    fn check_default(&self, e: &Expr, ty: ColumnType, what: &str) -> Result<()> {
        // A sequence's value or the clock is a DEFAULT alone, or cast; never computed with.
        let mut top = e;
        while let Expr::Nested(x) | Expr::Cast { expr: x, .. } = top {
            top = x;
        }
        let mut within = false;
        each_expr(e, &mut |x| {
            within |= !std::ptr::eq(x, top) && crate::catalog::hands_out(x)
        });
        if within && !crate::catalog::hands_out(top)
            || (within && !matches!(top, Expr::Function(_)))
        {
            return Err(Error::new(format!(
                "{what}: a DEFAULT that computes with nextval or the clock is not read: give it alone"
            )));
        }
        let given = self.default_type(e, what)?;
        if let Some(g) = given {
            if !assignable(g, ty) {
                return Err(Error::new(format!(
                    "{what} is of type {} but default expression is of type {}",
                    ty.pg_name(),
                    g.pg_name()
                )));
            }
        }
        Ok(())
    }

    /// The type a DEFAULT hands out where a function decides it; none for an expression of
    /// literals, whose type its column decides.
    fn default_type(&self, e: &Expr, what: &str) -> Result<Option<ColumnType>> {
        Ok(match e {
            Expr::Value(_) => None,
            Expr::Nested(x) | Expr::UnaryOp { expr: x, .. } => self.default_type(x, what)?,
            Expr::Cast { expr, .. } => {
                self.default_type(expr, what)?;
                None
            }
            Expr::BinaryOp { left, right, .. } => {
                self.default_type(left, what)?;
                self.default_type(right, what)?;
                None
            }
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) => {
                return Err(Error::new(format!(
                    "{what}: cannot use column reference in DEFAULT expression"
                )))
            }
            Expr::Function(f) => {
                let name = object_name(&f.name).to_lowercase();
                let name = name.strip_prefix("pg_catalog.").unwrap_or(&name);
                let args: Vec<&Expr> = match &f.args {
                    FunctionArguments::List(l) => l
                        .args
                        .iter()
                        .map(|a| match a {
                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                            other => Err(Error::new(format!(
                                "{what}: the argument {other} is not read"
                            ))),
                        })
                        .collect::<Result<_>>()?,
                    _ => Vec::new(),
                };
                match (name, args.as_slice()) {
                    ("nextval", [s]) => {
                        // A sequence that does not exist in PostgreSQL's words, as it says them.
                        self.nextval_of(s).map_err(|e| match e.sqlstate() {
                            Some(_) => e,
                            None => Error::new(format!("{what}: {e}")),
                        })?;
                        Some(ColumnType::Int8)
                    }
                    (
                        "now"
                        | "current_timestamp"
                        | "transaction_timestamp"
                        | "statement_timestamp"
                        | "clock_timestamp",
                        [],
                    ) => Some(ColumnType::Timestamptz),
                    ("gen_random_uuid" | "uuidv7" | "uuidv4", []) => Some(ColumnType::Uuid),
                    ("uuidv7", [_]) => {
                        return Err(Error::new(format!(
                            "{what}: {e} shifts its instant by an interval, which is not held"
                        )))
                    }
                    ("mod" | "abs", _) => {
                        for a in args {
                            self.default_type(a, what)?;
                        }
                        None
                    }
                    // A function the DDL declares, called as a statement calls it.
                    _ if self.functions.iter().any(|g| {
                        g.name == name
                            || !name.contains('.')
                                && g.name.strip_prefix("public.").unwrap_or(&g.name) == name
                    }) =>
                    {
                        for a in &args {
                            self.default_type(a, what)?;
                        }
                        self.functions
                            .iter()
                            .find(|g| {
                                g.name == name
                                    || g.name.strip_prefix("public.").unwrap_or(&g.name) == name
                            })
                            .map(|g| g.returns)
                    }
                    _ => {
                        return Err(Error::new(format!(
                            "{what}: the DEFAULT calls {}, which the snail does not read",
                            f.name
                        )))
                    }
                }
            }
            Expr::Case {
                conditions,
                else_result,
                ..
            } => {
                for c in conditions {
                    self.default_type(&c.condition, what)?;
                    self.default_type(&c.result, what)?;
                }
                if let Some(x) = else_result {
                    self.default_type(x, what)?;
                }
                None
            }
            other => {
                return Err(Error::new(format!(
                    "{what}: the DEFAULT {other} is not read"
                )))
            }
        })
    }

    fn sequence_mut(&mut self, name: &str) -> Option<&mut Sequence> {
        self.sequences.iter_mut().find(|s| s.name == name)
    }

    /// A sequence a serial or an identity column makes: named for its table and column, in the
    /// table's schema, unless it is named.
    fn column_sequence(
        &mut self,
        table: &str,
        column: &str,
        ty: SequenceType,
        options: &[SequenceOption],
        identity: bool,
    ) -> Result<String> {
        let schema = schema_of(table).to_string();
        let named = options.iter().find_map(|o| match o {
            SequenceOption::SequenceName(n) => Some(n.clone()),
            _ => None,
        });
        let name = match named {
            Some(n) if n.contains('.') => n,
            Some(n) => qualified(&schema, &n),
            None => {
                let t = bare(table).to_string();
                qualified(
                    &schema,
                    &choose(&t, Some(column), "seq", &|n| self.relation(&schema, n)),
                )
            }
        };
        if self.relation(schema_of(&name), bare(&name)) {
            return Err(Error::new(format!("relation \"{}\" already exists", bare(&name))));
        }
        let mut given: Vec<SequenceOption> = vec![SequenceOption::As(ty)];
        given.extend(
            options
                .iter()
                .filter(|o| !matches!(o, SequenceOption::SequenceName(_)))
                .cloned(),
        );
        let mut s = Sequence::made(&name, &given)?;
        s.owned_by = Some((table.to_string(), column.to_string()));
        s.identity = identity;
        self.push_sequence(s);
        Ok(name)
    }

    /// Reads one statement into the draft, or refuses it by name. Returns what it says of what it
    /// read and did nothing with.
    fn apply(&mut self, tokens: &[Token]) -> Result<Vec<String>> {
        {
            let (kind, object) = head(tokens);
            self.schema_named(&kind, &object, tokens)?;
        }
        if let Some((table, index)) = cluster_on(tokens) {
            let t = self.table_mut(&table).ok_or_else(|| {
                Error::new(format!(
                    "CLUSTER ON names {table}, which the DDL does not create"
                ))
            })?;
            if t.clustered.is_some() {
                return Err(Error::new(format!("{table} is clustered on two indexes")));
            }
            match t.keys().find(|k| k.name == index) {
                None => {
                    return Err(Error::new(format!(
                        "{table} is clustered on {index}, which is no index of it"
                    )))
                }
                Some(k) if !k.method.clusters() => {
                    return Err(Error::new(format!(
                        "cannot cluster on index \"{index}\" because access method does not \
                         support clustering"
                    )))
                }
                Some(k) if k.predicate.is_some() => {
                    return Err(Error::new(format!(
                        "cannot cluster on partial index \"{index}\""
                    )))
                }
                Some(k)
                    if k.opclasses
                        .iter()
                        .any(|o| crate::methods::holds_elements(k.method, o)) =>
                {
                    return Err(Error::new(format!(
                        "{table} cannot be clustered on {index}: its strand holds a row once per \
                         element, and the table's order holds it once"
                    )))
                }
                Some(_) => {}
            }
            t.clustered = Some(index);
            return Ok(Vec::new());
        }
        if let Some(form) = read(tokens) {
            return self.form(form?);
        }
        if let Some((kept, extras, fks, temporal)) = create_table_extras(tokens)? {
            return self.create_table(&kept, extras, fks, &temporal);
        }
        let (kind, object) = head(tokens);
        if kind == "CREATE INDEX" || kind == "CREATE UNIQUE INDEX" {
            return self.create_index(tokens);
        }
        if kind == "ALTER TABLE" {
            return self.alter_table(tokens, &object);
        }
        // A view's body read as a statement's is, its ROWS FROM and its functions' column
        // definition lists as `spelled` writes them; its WITH CHECK OPTION, which sqlparser drops,
        // read here.
        let (tokens, check) = match kind == "CREATE VIEW" || kind == "CREATE OR REPLACE VIEW" {
            true => check_option(&view_scanned(tokens)?),
            false => (tokens.to_vec(), None),
        };
        // A function's body in SQL's own words, which sqlparser reads otherwise or not at all,
        // taken out of the words here.
        let (tokens, sql) = match kind.starts_with("CREATE") && kind.ends_with("FUNCTION") {
            true => sql_body(&tokens)?,
            false => (tokens, None),
        };
        let tokens = tokens.as_slice();
        let st = Parser::new(&crate::grammar::POSTGRES)
            .with_tokens(tokens.to_vec())
            .parse_statement()
            .map_err(|e| {
                Error::new(format!(
                    "{kind} {object}: sqlparser 0.63 reads no such statement ({e})"
                ))
            })?;
        match &st {
            Statement::CreateFunction(f) => {
                // sqlparser writes a parameter's DEFAULT as `=`.
                let compared: Vec<Token> = tokens
                    .iter()
                    .map(|t| match t {
                        Token::Word(w)
                            if w.quote_style.is_none() && w.value.eq_ignore_ascii_case("default") =>
                        {
                            Token::Eq
                        }
                        t => t.clone(),
                    })
                    .collect();
                read_as_written(&compared, &st, &format!("{kind} {object}"))?;
                let name = object_name(&f.name);
                let held = |dt: &DataType| match self.held(dt, &name)? {
                    (t, None) => Ok(t),
                    (_, Some(d)) => Err(Error::new(format!(
                        "{name}: a parameter or result of the domain {d} is not read"
                    ))),
                };
                let f = function(f, sql, &held, &self.relations_read())?;
                if let Some(g) = self.functions.iter().find(|g| g.name == f.name) {
                    let last = f.name.rsplit('.').next().unwrap_or(&f.name);
                    return Err(Error::new(match g.args == f.args {
                        true => format!(
                            "function \"{last}\" already exists with same argument types"
                        ),
                        false => format!(
                            "function {}: a second function of its name is not read",
                            f.name
                        ),
                    }));
                }
                self.parameter_defaults(&f)?;
                // Its body read as PostgreSQL reads it once the function is made: typed with the
                // functions made before it and itself, its subqueries' names over the relations
                // made before it, and its type one RETURNS takes.
                let relations = self.relations_read();
                let body =
                    crate::read::own::body_type(&self.functions, &f, self.release, &relations)?;
                if let Some(t) = body {
                    if t != f.returns && !crate::catalog::assignable(t, f.returns) {
                        return Err(Error::new(format!(
                            "return type mismatch in function declared to return {}\nDETAIL:  \
                             Actual return type is {}.",
                            f.returns.pg_name(),
                            t.pg_name()
                        )));
                    }
                }
                self.functions.push(f);
                Ok(Vec::new())
            }
            Statement::CreateView(v) => {
                // Its labels as PostgreSQL's grammar reads them, a label without AS sqlparser
                // prints after one.
                let bare = crate::read::ddl_labels(&written(tokens))?;
                read_as_labelled(tokens, &st, &format!("{kind} {object}"), bare)?;
                let _own = crate::read::own::holding(&self.functions, self.release);
                self.create_view(v, check)
            }
            _ => Err(Error::new(format!(
                "the statement {kind} {object} is not read"
            ))),
        }
    }

    /// Each DEFAULT of a function's parameters read as PostgreSQL's parse analysis reads it when
    /// the function is made, over the objects made before it: no subquery, aggregate, window or
    /// set-returning function, no column, and a value its parameter takes by assignment.
    fn parameter_defaults(&self, f: &Function) -> Result<()> {
        use crate::deparse::analyze::{Analyzer, Context, Form};
        if f.defaults.is_empty() {
            return Ok(());
        }
        let catalog = Catalog {
            release: self.release,
            ..self.so_far()
        };
        let _named = crate::oids::named(catalog.oids.clone());
        let cat = crate::deparse::cat::Cat::of(&catalog);
        let a = Analyzer {
            cat: &cat,
            columns: Vec::new(),
            value: None,
            scope: None,
        };
        for (e, &ty) in f.defaults.iter().zip(&f.args[f.required()..]) {
            let mut why = None;
            let _ = sqlparser::ast::visit_expressions(e, |x| {
                let found = match x {
                    Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
                        Some("cannot use subquery in DEFAULT expression")
                    }
                    Expr::Function(g) if g.over.is_some() => {
                        Some("window functions are not allowed in DEFAULT expressions")
                    }
                    Expr::Function(g) => {
                        let n = object_name(&g.name);
                        let n = n.strip_prefix("pg_catalog.").unwrap_or(&n);
                        if crate::read::pipeline::is_aggregate(n) {
                            Some("aggregate functions are not allowed in DEFAULT expressions")
                        } else if crate::read::pipeline::is_set_returning(n) || crate::read::own::returns_set(n, self.release) {
                            Some("set-returning functions are not allowed in DEFAULT expressions")
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                match found {
                    Some(w) => {
                        why = Some(w);
                        std::ops::ControlFlow::Break(())
                    }
                    None => std::ops::ControlFlow::Continue(()),
                }
            });
            if let Some(w) = why {
                return Err(Error::new(w));
            }
            let node = a.expr(e)?;
            let target = crate::pgcatalog::type_oid_of(&catalog, ty, None);
            if !a.can_coerce_all(&[node.ty()], &[target], Context::Assignment) {
                return Err(Error::new(format!(
                    "argument of DEFAULT must be type {}, not type {}",
                    a.type_name(target),
                    a.type_name(node.ty())
                )));
            }
            a.coerce_to_target(node, target, -1, Context::Assignment, Form::Implicit)?;
        }
        Ok(())
    }

    /// A view: its question read over the tables and the views declared before it, as
    /// PostgreSQL's parser reads it when the view is made, and kept as written.
    fn create_view(
        &mut self,
        v: &sqlparser::ast::CreateView,
        check_option: Option<&'static str>,
    ) -> Result<Vec<String>> {
        let name = object_name(&v.name);
        let what = format!("CREATE VIEW {name}");
        let refused = |clause: &str| Err(Error::new(format!("{what}: {clause} is not read")));
        if v.materialized {
            return refused("MATERIALIZED");
        }
        if v.temporary {
            return refused("TEMPORARY");
        }
        if v.or_alter
            || v.secure
            || v.to.is_some()
            || v.params.is_some()
            || !v.cluster_by.is_empty()
            || v.comment.is_some()
            || v.with_no_schema_binding
            || v.copy_grants
            || v.if_not_exists
            || !matches!(v.options, sqlparser::ast::CreateTableOptions::None)
        {
            return refused("a clause beside its name, its columns and its question");
        }
        if self.relation(schema_of(&name), bare(&name)) || self.composite_named(&name) {
            if v.or_replace && self.views.iter().any(|x| x.name == name) {
                return refused("OR REPLACE of a view that exists");
            }
            return Err(Error::new(format!("relation \"{}\" already exists", bare(&name))));
        }
        if self.type_named(&name) {
            return Err(row_type_taken(&name));
        }
        let columns = v
            .columns
            .iter()
            .map(|c| match (&c.data_type, &c.options) {
                (None, None) => Ok(ident(&c.name)),
                _ => Err(Error::new(format!("{what}: the column {c} is not read"))),
            })
            .collect::<Result<Vec<String>>>()?;
        let made = crate::read::view_columns(&self.so_far(), &crate::grammar::query(&v.query))?;
        if columns.len() > made.len() {
            return Err(Error::new(
                "CREATE VIEW specifies more column names than columns",
            ));
        }
        let mut named = made;
        for (n, c) in named.iter_mut().zip(&columns) {
            *n = c.clone();
        }
        if let Some(n) =
            (0..named.len()).find_map(|i| named[..i].contains(&named[i]).then_some(&named[i]))
        {
            return Err(Error::new(format!(
                "column \"{n}\" specified more than once"
            )));
        }
        let view = View {
            name,
            columns,
            query: v.query.to_string(),
            check_option,
        };
        // A CHECK OPTION stands only on a view a write can go through.
        if check_option.is_some()
            && crate::read::updatable::through(&self.so_far(), &view)?.is_err()
        {
            return Err(Error::new(
                "WITH CHECK OPTION is supported only on automatically updatable views",
            ));
        }
        count_in(&mut self.relations, in_schema(schema_of(&view.name), bare(&view.name)));
        self.views.push(view);
        Ok(Vec::new())
    }

    /// The catalog as the DDL has declared it so far: the tables that are whole, and every other
    /// object.
    fn so_far(&self) -> Catalog {
        Catalog {
            ddl: String::new(),
            tables: self.finish().into_iter().filter_map(|t| t.ok()).collect(),
            functions: self.functions.clone(),
            types: self.types.clone(),
            domains: self.domains.clone(),
            sequences: self.sequences.clone(),
            collations: self.collations.clone(),
            views: self.views.clone(),
            release: crate::regs::LATEST,
            monetary: crate::monetary::Monetary::C,
            text_search: crate::tsearch::TextSearch::Simple,
            collation: String::new(),
            oids: std::sync::Arc::new(self.oids.clone()),
        }
    }

    /// Refused where a collation a statement names is not in the file, neither PostgreSQL's own
    /// under the file's release nor one its DDL made, as PostgreSQL refuses a name it does not find.
    fn collation_exists(&self, name: &str) -> Result<()> {
        let declared = self.collations.iter().any(|c| c.name == name)
            || crate::catalog::bytes_casing(name, &self.collations).is_some();
        if declared || crate::pgcatalog::is_built_in_collation(name, self.release) {
            return Ok(());
        }
        Err(Error::new(format!(
            "collation \"{name}\" for encoding \"UTF8\" does not exist"
        )))
    }

    fn form(&mut self, form: Form) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        match form {
            Form::Comment { target, text } => {
                let (objoid, classoid, column) = self.commented(&target)?;
                self.described.retain(|d| {
                    !(d.objoid == objoid && d.classoid == classoid && d.column == column)
                });
                // An empty text removes the description, as NULL does.
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    self.described.push(Description {
                        objoid,
                        classoid,
                        column,
                        text,
                    });
                }
            }
            Form::CreateType(t) => {
                if self.type_named(&t.name) {
                    return Err(type_taken(&t.name));
                }
                if let TypeKind::Enum { labels } = &t.kind {
                    if let Some(l) = labels.iter().find(|l| l.len() > NAMEDATALEN - 1) {
                        return Err(Error::new(format!(
                            "{}: invalid enum label \"{l}\": labels must be {} bytes or less",
                            t.name,
                            NAMEDATALEN - 1
                        )));
                    }
                }
                if let TypeKind::Range(r) = &t.kind {
                    if self.type_named(&r.multirange) {
                        return Err(type_taken(&r.multirange));
                    }
                    if let Some(k) = &r.collation {
                        self.collation_exists(k)?;
                    }
                }
                if let TypeKind::Composite { fields } = &t.kind {
                    for k in fields.iter().filter_map(|f| f.2.as_ref()) {
                        self.collation_exists(k)?;
                    }
                }
                self.types.push(t);
            }
            Form::AlterType(name, change) => {
                let t = self
                    .types
                    .iter_mut()
                    .find(|t| t.name == name)
                    .ok_or_else(|| Error::new(format!("type \"{name}\" does not exist")))?;
                let TypeKind::Enum { labels } = &mut t.kind else {
                    return Err(Error::new(format!("{name} is not an enum")));
                };
                let was = crate::composed::type_of(crate::composed::Composed::Enum {
                    name: name.clone(),
                    labels: labels.clone(),
                });
                match change {
                    AlterType::AddValue {
                        label,
                        if_not_exists,
                        beside,
                    } => {
                        if labels.contains(&label) {
                            if if_not_exists {
                                notices.push(format!(
                                    "enum label \"{label}\" already exists, skipping"
                                ));
                                return Ok(notices);
                            }
                            return Err(Error::new(format!(
                                "enum label \"{label}\" already exists"
                            )));
                        }
                        if label.len() > NAMEDATALEN - 1 {
                            return Err(Error::new(format!("invalid enum label \"{label}\"")));
                        }
                        let at = match beside {
                            None => labels.len(),
                            Some((before, other)) => {
                                let i =
                                    labels.iter().position(|l| *l == other).ok_or_else(|| {
                                        Error::new(format!(
                                            "\"{other}\" is not an existing enum label"
                                        ))
                                    })?;
                                if before {
                                    i
                                } else {
                                    i + 1
                                }
                            }
                        };
                        labels.insert(at, label);
                    }
                    AlterType::RenameValue { from, to } => {
                        if labels.contains(&to) {
                            return Err(Error::new(format!("enum label \"{to}\" already exists")));
                        }
                        let i = labels.iter().position(|l| *l == from).ok_or_else(|| {
                            Error::new(format!("\"{from}\" is not an existing enum label"))
                        })?;
                        let enum_name = crate::oids::full(&name);
                        self.renamed.push((
                            crate::oids::Object::Label(enum_name.clone(), from.clone()),
                            crate::oids::Object::Label(enum_name, to.clone()),
                        ));
                        labels[i] = to;
                    }
                }
                // Every column of the enum, or of a type holding it, takes its labels as they now stand.
                let now = crate::composed::type_of(crate::composed::Composed::Enum {
                    name: name.clone(),
                    labels: labels.clone(),
                });
                for t in &mut self.tables {
                    for c in &mut t.columns {
                        c.ty = crate::composed::replace(c.ty, was, now);
                    }
                }
            }
            Form::CreateDomain(mut d) => {
                if self.type_named(&d.name) {
                    return Err(type_taken(&d.name));
                }
                if let Some(k) = &d.collation {
                    self.collation_exists(k)?;
                }
                let schema = schema_of(&d.name).to_string();
                let domain = bare(&d.name).to_string();
                let mut used: Vec<String> = Vec::new();
                for k in &mut d.checks {
                    if k.name.is_empty() {
                        k.name = choose(&domain, None, "check", &|n| {
                            self.constraint(&schema, n) || used.iter().any(|u| u == n)
                        });
                    } else if used.contains(&k.name) {
                        return Err(Error::new(format!(
                            "constraint \"{}\" for domain \"{}\" already exists",
                            k.name, d.name
                        )));
                    }
                    used.push(k.name.clone());
                }
                for n in used {
                    self.push_check_name(qualified(&schema, &n));
                }
                self.domains.push(d);
            }
            Form::AlterDomain(name, change) => {
                let schema = schema_of(&name).to_string();
                let domain = bare(&name).to_string();
                let taken: Vec<String> = self.check_names.clone();
                let tables_taken = |n: &str| {
                    taken.contains(&qualified(&schema, n))
                        || self.tables.iter().any(|t| {
                            schema_of(&t.name) == schema
                                && t.primary
                                    .iter()
                                    .chain(t.uniques.iter())
                                    .any(|k| k.name == n)
                        })
                };
                let chosen = choose(&domain, None, "check", &tables_taken);
                let d = self
                    .domains
                    .iter_mut()
                    .find(|d| d.name == name)
                    .ok_or_else(|| Error::new(format!("type \"{name}\" does not exist")))?;
                match change {
                    AlterDomain::SetDefault(e) => d.default = Some(e),
                    AlterDomain::DropDefault => d.default = None,
                    AlterDomain::SetNotNull => d.not_null = true,
                    AlterDomain::DropNotNull => {
                        d.not_null = false;
                        d.not_null_name = None;
                    }
                    AlterDomain::AddNotNull { name: n } => {
                        d.not_null = true;
                        d.not_null_name = n;
                    }
                    AlterDomain::AddCheck {
                        name: n,
                        check,
                        valid,
                    } => {
                        let n = match n {
                            Some(n) => {
                                if d.checks.iter().any(|k| k.name == n) {
                                    return Err(Error::new(format!(
                                        "constraint \"{n}\" for domain \"{name}\" already exists"
                                    )));
                                }
                                n
                            }
                            None => chosen,
                        };
                        self.push_check_name(qualified(&schema, &n));
                        let d = self.domains.iter_mut().find(|d| d.name == name).unwrap();
                        d.checks.push(DomainCheck {
                            name: n,
                            check,
                            valid,
                        });
                    }
                    AlterDomain::DropConstraint { name: n, if_exists } => {
                        match d.checks.iter().position(|k| k.name == n) {
                            Some(i) => {
                                d.checks.remove(i);
                                let q = qualified(&schema, &n);
                                let before = self.check_names.len();
                                self.check_names.retain(|x| *x != q);
                                for _ in self.check_names.len()..before {
                                    count_out(&mut self.check_named, &q);
                                }
                            }
                            None if d.not_null_name.as_deref() == Some(n.as_str()) => {
                                d.not_null = false;
                                d.not_null_name = None;
                            }
                            None if if_exists => notices.push(format!(
                                "constraint \"{n}\" of domain \"{name}\" does not exist, skipping"
                            )),
                            None => {
                                return Err(Error::new(format!(
                                    "constraint \"{n}\" of domain \"{name}\" does not exist"
                                )))
                            }
                        }
                    }
                    AlterDomain::RenameConstraint { from, to } => {
                        if d.checks.iter().any(|k| k.name == to) {
                            return Err(Error::new(format!(
                                "constraint \"{to}\" for domain \"{name}\" already exists"
                            )));
                        }
                        let k = d
                            .checks
                            .iter_mut()
                            .find(|k| k.name == from)
                            .ok_or_else(|| {
                                Error::new(format!(
                                    "constraint \"{from}\" of domain \"{name}\" does not exist"
                                ))
                            })?;
                        k.name = to.clone();
                        let q = qualified(&schema, &from);
                        for x in &mut self.check_names {
                            if *x == q {
                                count_out(&mut self.check_named, &q);
                                *x = qualified(&schema, &to);
                                count_in(&mut self.check_named, x.clone());
                            }
                        }
                        let domain_name = crate::oids::full(&name);
                        self.renamed.push((
                            crate::oids::Object::Constraint(domain_name.clone(), from.clone()),
                            crate::oids::Object::Constraint(domain_name, to.clone()),
                        ));
                    }
                    AlterDomain::ValidateConstraint(n) => {
                        let k = d.checks.iter_mut().find(|k| k.name == n).ok_or_else(|| {
                            Error::new(format!(
                                "constraint \"{n}\" of domain \"{name}\" does not exist"
                            ))
                        })?;
                        k.valid = true;
                    }
                }
            }
            Form::CreateSequence {
                mut sequence,
                if_not_exists,
                options,
            } => {
                if self.relation(schema_of(&sequence.name), bare(&sequence.name))
                    || self.composite_named(&sequence.name)
                {
                    if if_not_exists {
                        notices.push(format!(
                            "relation \"{}\" already exists, skipping",
                            bare(&sequence.name)
                        ));
                        return Ok(notices);
                    }
                    return Err(Error::new(format!(
                        "relation \"{}\" already exists",
                        bare(&sequence.name)
                    )));
                }
                if self.type_named(&sequence.name) {
                    return Err(row_type_taken(&sequence.name));
                }
                sequence.owned_by = self.owner(&sequence.name, &options, None)?;
                self.push_sequence(sequence);
            }
            Form::AlterSequence {
                name,
                if_exists,
                options,
            } => {
                let owned = self
                    .sequences
                    .iter()
                    .find(|s| s.name == name)
                    .map(|s| s.owned_by.clone());
                let Some(owned) = owned else {
                    if if_exists {
                        notices.push(format!("relation \"{name}\" does not exist, skipping"));
                        return Ok(notices);
                    }
                    return Err(Error::new(format!("relation \"{name}\" does not exist")));
                };
                let owned_by = self.owner(&name, &options, owned)?;
                let s = self.sequence_mut(&name).unwrap();
                s.alter(&options)?;
                s.owned_by = owned_by;
            }
            Form::CreateCollation {
                collation,
                if_not_exists,
            } => {
                if self.collations.iter().any(|c| c.name == collation.name) {
                    if if_not_exists {
                        notices.push(format!(
                            "collation \"{}\" already exists, skipping",
                            collation.name
                        ));
                        return Ok(notices);
                    }
                    return Err(Error::new(format!(
                        "collation \"{}\" already exists",
                        collation.name
                    )));
                }
                let mut collation = collation;
                if !self.kept {
                    notices.extend(made_collation(&mut collation, &self.collations)?);
                }
                self.collations.push(collation);
            }
            Form::CreateSchema {
                name,
                if_not_exists,
            } => {
                if name.starts_with("pg_") {
                    return Err(Error::new(format!(
                        "unacceptable schema name \"{name}\": The prefix \"pg_\" is reserved for \
                         system schemas."
                    )));
                }
                if self.schema_stands(&name) {
                    if if_not_exists {
                        notices.push(format!("schema \"{name}\" already exists, skipping"));
                        return Ok(notices);
                    }
                    return Err(Error::new(format!("schema \"{name}\" already exists")));
                }
                self.schemas.push(name);
            }
            Form::DropSchema {
                names,
                if_exists,
                cascade,
            } => return self.drop_schemas(&names, if_exists, cascade),
            Form::DropRelations {
                kind,
                names,
                if_exists,
                cascade,
            } => return self.drop_relations(kind, &names, if_exists, cascade),
            Form::DropTypes {
                domain,
                names,
                if_exists,
                cascade,
            } => return self.drop_types(domain, &names, if_exists, cascade),
            Form::DropFunctions {
                names,
                if_exists,
                cascade,
            } => return self.drop_functions(&names, if_exists, cascade),
        }
        Ok(notices)
    }

    /// One of pg_catalog's relations of that name, as the release's catalog holds it: its OID and
    /// its kind.
    fn built_in_relation(&self, name: &str) -> Option<(u32, char)> {
        crate::pgcatalog::built_in("pg_class", self.release)
            .iter()
            .find(|r| {
                r.get("relnamespace").cloned().flatten().as_deref() == Some("11")
                    && r.get("relname").cloned().flatten().as_deref() == Some(name)
            })
            .and_then(|r| {
                let oid = r.get("oid").cloned().flatten()?.parse().ok()?;
                Some((oid, r.get("relkind").cloned().flatten()?.chars().next()?))
            })
    }

    /// The kind of the file's relation of that name in that schema, as pg_class's relkind gives it.
    fn relation_kind(&self, schema: &str, own: &str) -> Option<char> {
        use crate::oids::full;
        let name = format!("{schema}.{own}");
        if self.tables.iter().any(|t| full(&t.name) == name) {
            return Some('r');
        }
        if self.views.iter().any(|v| full(&v.name) == name) {
            return Some('v');
        }
        if self.sequences.iter().any(|x| full(&x.name) == name) {
            return Some('S');
        }
        if self
            .tables
            .iter()
            .any(|t| schema_of(&t.name) == schema && t.keys().any(|k| k.name == own))
        {
            return Some('i');
        }
        self.types
            .iter()
            .any(|t| full(&t.name) == name && matches!(t.kind, TypeKind::Composite { .. }))
            .then_some('c')
    }

    /// DROP TABLE, VIEW, SEQUENCE or INDEX: each relation named, found as PostgreSQL's search path
    /// finds a name with no schema (pg_catalog's own first, then public), refused where it is of
    /// another kind; and under CASCADE every object that depends on one, refused under RESTRICT
    /// where an object depends on one.
    fn drop_relations(
        &mut self,
        kind: Dropped,
        names: &[(Option<String>, String)],
        if_exists: bool,
        cascade: bool,
    ) -> Result<Vec<String>> {
        use crate::oids::Object as O;
        let (word, one) = (kind.word(), kind.one());
        let mut notices = Vec::new();
        let mut named = Vec::new();
        for (schema, own) in names {
            if schema.as_deref().is_none_or(|s| s == "pg_catalog") {
                if let Some((oid, k)) = self.built_in_relation(own) {
                    return Err(Error::new(match k == kind.relkind() {
                        false => format!("\"{own}\" is not {one}"),
                        true if oid < 12000 => {
                            format!("permission denied: \"{own}\" is a system catalog")
                        }
                        true => format!(
                            "DROP {} pg_catalog.{own} is refused: PostgreSQL's own relations are \
                             the release's, which a snail carries beside its file, not objects its \
                             DDL made",
                            word.to_uppercase()
                        ),
                    }));
                }
            }
            if schema.as_deref() == Some("information_schema") {
                return Err(Error::new(format!(
                    "DROP {} of information_schema's is refused: its relations are the release's \
                     own, which a snail carries beside its file, not objects its DDL made",
                    word.to_uppercase()
                )));
            }
            let s = schema.clone().unwrap_or_else(|| "public".to_string());
            if schema.is_some() && !self.schema_stands(&s) {
                if !if_exists {
                    return Err(Error::new(format!("schema \"{s}\" does not exist")));
                }
                notices.push(format!("schema \"{s}\" does not exist, skipping"));
                continue;
            }
            match self.relation_kind(&s, own) {
                Some(k) if k == kind.relkind() => {
                    let oid = self.oids.of(&O::Class(format!("{s}.{own}"))).unwrap_or(0);
                    named.push(crate::deparse::depend::Addr::of(
                        crate::deparse::depend::CLASS,
                        oid,
                    ));
                }
                Some(_) => return Err(Error::new(format!("\"{own}\" is not {one}"))),
                None if if_exists => {
                    notices.push(format!("{word} \"{own}\" does not exist, skipping"))
                }
                None => return Err(Error::new(format!("{word} \"{own}\" does not exist"))),
            }
        }
        self.dropping(&named, cascade, notices)
    }

    /// DROP TYPE or DROP DOMAIN: each type named, found as a type's name is read (pg_catalog's own
    /// first, then the file's), a DROP DOMAIN's refused where it is no domain; and under CASCADE
    /// every object that depends on one, refused under RESTRICT where an object depends on one.
    /// PostgreSQL's own types, an array and a table's row type are refused as PostgreSQL refuses
    /// them, by what requires them.
    fn drop_types(
        &mut self,
        domain: bool,
        names: &[TypeNamed],
        if_exists: bool,
        cascade: bool,
    ) -> Result<Vec<String>> {
        use crate::oids::{full, Object as O};
        let mut notices = Vec::new();
        let mut named = Vec::new();
        let held = crate::oids::named(std::sync::Arc::new(self.oids.clone()));
        for n in names {
            if let Some(s) = n.schema.as_deref().filter(|s| !self.schema_stands(s)) {
                if !if_exists {
                    return Err(Error::new(format!("schema \"{s}\" does not exist")));
                }
                notices.push(format!("schema \"{s}\" does not exist, skipping"));
                continue;
            }
            let oid = match crate::regs::type_oid(&n.words, self.release) {
                Ok(oid) => oid,
                Err(e) if e.sqlstate() == Some("42704") => {
                    if !if_exists {
                        return Err(Error::new(format!("type \"{}\" does not exist", n.shown)));
                    }
                    notices.push(format!("type \"{}\" does not exist, skipping", n.shown));
                    continue;
                }
                Err(e) => return Err(e),
            };
            let is_domain = self.domains.iter().any(|x| {
                self.oids.of(&O::Type(full(&x.name))) == Some(oid)
            });
            if domain && !is_domain {
                return Err(Error::new(format!("\"{}\" is not a domain", n.shown)));
            }
            named.push(crate::deparse::depend::Addr::of(crate::deparse::depend::TYPE, oid));
        }
        drop(held);
        self.dropping(&named, cascade, notices)
    }

    /// DROP FUNCTION: each function named, found as PostgreSQL's search path finds a name with no
    /// schema (pg_catalog's own first, then the file's): by its arguments' types where it lists
    /// them, else the one function of its name, refused where none or several bear it; and under
    /// CASCADE every object that depends on one, refused under RESTRICT where an object depends on
    /// one. PostgreSQL's own functions are refused as the system's.
    fn drop_functions(
        &mut self,
        names: &[FunctionNamed],
        if_exists: bool,
        cascade: bool,
    ) -> Result<Vec<String>> {
        use crate::oids::{full, parts, Object as O};
        let mut notices = Vec::new();
        let mut named = Vec::new();
        for n in names {
            let f = full(&n.name);
            let (s, own) = parts(&f);
            let qualified = n.name.contains('.');
            if qualified && !self.schema_stands(s) {
                if !if_exists {
                    return Err(Error::new(format!("schema \"{s}\" does not exist")));
                }
                notices.push(format!("schema \"{s}\" does not exist, skipping"));
                continue;
            }
            // pg_catalog's, which the search path finds first; the file's own in its schema.
            let system = !qualified || s == "pg_catalog";
            let file = s != "pg_catalog";
            let oid = match &n.args {
                Some(args) => {
                    let mut types = Vec::new();
                    for a in args {
                        types.push(self.held(a, "DROP FUNCTION")?.0.pg_name());
                    }
                    let joined = types.join(",");
                    let built_in = system
                        .then(|| {
                            crate::regs::reg_parse(
                                crate::regs::RegKind::Procedure,
                                &format!("pg_catalog.{own}({joined})"),
                                self.release,
                            )
                            .ok()
                        })
                        .flatten();
                    let found = built_in.or_else(|| {
                        file.then(|| self.oids.of(&O::Proc(f.clone(), joined.clone())))
                            .flatten()
                    });
                    match found {
                        Some(oid) => oid,
                        None if if_exists => {
                            let written: Vec<String> = args.iter().map(type_name_written).collect();
                            notices.push(format!(
                                "function {}({}) does not exist, skipping",
                                n.name,
                                written.join(",")
                            ));
                            continue;
                        }
                        None => {
                            return Err(Error::new(format!(
                                "function {}({}) does not exist",
                                n.name,
                                types.join(", ")
                            )))
                        }
                    }
                }
                None => {
                    let mut found: Vec<u32> = match system {
                        true => self.built_in_function_oids(own),
                        false => Vec::new(),
                    };
                    if file {
                        found.extend(
                            self.oids
                                .each()
                                .filter(|(_, o)| matches!(o, O::Proc(x, _) if *x == f))
                                .map(|(oid, _)| oid),
                        );
                    }
                    match found.as_slice() {
                        [oid] => *oid,
                        [] if if_exists => {
                            notices.push(format!("function {}() does not exist, skipping", n.name));
                            continue;
                        }
                        [] => {
                            return Err(Error::new(format!(
                                "could not find a function named \"{}\"",
                                n.name
                            )))
                        }
                        _ => {
                            return Err(Error::new(format!(
                                "function name \"{}\" is not unique\nHINT:  Specify the argument \
                                 list to select the function unambiguously.",
                                n.name
                            )))
                        }
                    }
                }
            };
            named.push(crate::deparse::depend::Addr::of(crate::deparse::depend::PROC, oid));
        }
        self.dropping(&named, cascade, notices)
    }

    /// What a DROP of the objects named takes, as PostgreSQL's dependency walk finds it over
    /// pg_depend's rows, taken out of the draft; with what it says beside `notices`.
    fn dropping(
        &mut self,
        named: &[crate::deparse::depend::Addr],
        cascade: bool,
        mut notices: Vec<String>,
    ) -> Result<Vec<String>> {
        if named.is_empty() {
            return Ok(notices);
        }
        let catalog = Catalog {
            release: self.release,
            ..self.so_far()
        };
        let dropped = crate::cascade::dropped(&catalog, named, cascade)?;
        self.take_out(&catalog, &dropped.taken)?;
        notices.extend(dropped.notice);
        Ok(notices)
    }

    /// Whether a schema of that name stands: one CREATE SCHEMA made, or a file's kept statement
    /// named; or PostgreSQL's own, `public` among them until the DDL drops it.
    fn schema_stands(&self, name: &str) -> bool {
        self.schemas.iter().any(|s| s == name)
            || ((name != "public" || !self.oids.public_dropped())
                && self.built_in_schema(name).is_some())
    }

    /// The schema a statement makes its object in, or finds the table it indexes or alters in,
    /// refused where none of that name stands, as PostgreSQL refuses it before it reads the rest:
    /// one no statement made, or `public` dropped where a name has no schema. A file's own kept
    /// statements are read as they were written.
    fn schema_named(&self, kind: &str, object: &str, tokens: &[Token]) -> Result<()> {
        if self.kept {
            return Ok(());
        }
        let w: Vec<&str> = kind.split(' ').collect();
        let temporary = w.iter().any(|k| matches!(*k, "TEMP" | "TEMPORARY"));
        let made = w.first() == Some(&"CREATE")
            && !temporary
            && matches!(
                w.last().copied(),
                Some(
                    "TABLE" | "TYPE" | "DOMAIN" | "SEQUENCE" | "VIEW" | "FUNCTION" | "PROCEDURE"
                        | "COLLATION" | "AGGREGATE"
                )
            );
        let named = if made {
            Some(object.to_string())
        } else if w.first() == Some(&"CREATE") && w.last() == Some(&"INDEX") {
            index_on(&written(tokens))
        } else if kind == "ALTER TABLE" && !alters_if_exists(tokens) {
            Some(object.to_string())
        } else {
            None
        };
        match named.as_deref().map(|n| n.rsplit_once('.')) {
            Some(Some((s, _))) if !self.schema_stands(s) => {
                Err(Error::new(format!("schema \"{s}\" does not exist")))
            }
            Some(None) if made && !self.schema_stands("public") => {
                Err(Error::new("no schema has been selected to create in"))
            }
            _ => Ok(()),
        }
    }

    /// Refuses a statement whose objects lie in a schema that stands nowhere, as PostgreSQL refuses
    /// it: a name another clause gives an object, a multirange's among them. A file's own kept
    /// statements are read as they were written.
    fn no_schema_implied(&self, before: Lens) -> Result<()> {
        if self.kept {
            return Ok(());
        }
        for o in self.objects_since(before, &[]) {
            let s = match &o {
                crate::oids::Object::Namespace(n) => n.as_str(),
                other => crate::oids::parts(other.name()).0,
            };
            if s != "pg_catalog" && !self.schema_stands(s) {
                return Err(Error::new(format!("schema \"{s}\" does not exist")));
            }
        }
        Ok(())
    }

    /// The schemas a file's own kept statement named that no statement made: each stands from it
    /// on, as one made, whether or not an object still lies in it.
    fn keep_named_schemas(&mut self) {
        if !self.kept {
            return;
        }
        let named: Vec<String> = self
            .oids
            .each()
            .filter_map(|(_, o)| match o {
                crate::oids::Object::Namespace(n) if !self.schemas.contains(n) => Some(n.clone()),
                _ => None,
            })
            .collect();
        self.schemas.extend(named);
    }

    /// The OID of one of PostgreSQL's own schemas, as the release's catalog holds it.
    fn built_in_schema(&self, name: &str) -> Option<u32> {
        crate::pgcatalog::built_in("pg_namespace", self.release)
            .iter()
            .find(|r| r.get("nspname").cloned().flatten().as_deref() == Some(name))
            .and_then(|r| r.get("oid").cloned().flatten()?.parse().ok())
    }

    /// DROP SCHEMA: each schema named, and under CASCADE every object that depends on one, as
    /// PostgreSQL's dependency walk finds them over pg_depend's rows; refused under RESTRICT where an
    /// object depends on one. `public` goes as PostgreSQL drops it, a name with no schema then
    /// found only in pg_catalog; `information_schema`, the release's own, is refused by name.
    fn drop_schemas(
        &mut self,
        names: &[String],
        if_exists: bool,
        cascade: bool,
    ) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        let mut found: Vec<&str> = Vec::new();
        for n in names {
            if self.schema_stands(n) {
                found.push(n);
                continue;
            }
            if !if_exists {
                return Err(Error::new(format!("schema \"{n}\" does not exist")));
            }
            notices.push(format!("schema \"{n}\" does not exist, skipping"));
        }
        if found.is_empty() {
            return Ok(notices);
        }
        let mut named = Vec::new();
        let public = found.contains(&"public");
        for n in found {
            let oid = match n {
                "public" => self.oids.public().unwrap_or(crate::oids::PUBLIC),
                "information_schema" => {
                    return Err(Error::new(
                        "DROP SCHEMA information_schema is refused: its views are the release's \
                         own, which a snail carries beside its file, not objects its DDL made",
                    ))
                }
                _ => self
                    .oids
                    .of(&crate::oids::Object::Namespace(n.to_string()))
                    .or_else(|| self.built_in_schema(n))
                    .unwrap_or(0),
            };
            named.push(crate::deparse::depend::Addr::of(crate::deparse::depend::NAMESPACE, oid));
        }
        let notices = self.dropping(&named, cascade, notices)?;
        if public {
            // The release's public goes for good; one CREATE SCHEMA public makes again is the
            // file's own, and went with the drop.
            self.oids.drop_public();
            self.schemas.retain(|s| s != "public");
        }
        Ok(notices)
    }

    /// What a DROP takes, taken out of the draft: each whole object, and each part of an object it
    /// leaves that stands alone (a DEFAULT, a CHECK, a foreign key, an index, a domain's
    /// constraint). A column of a relation it leaves, or a key of a table it leaves, is refused by
    /// name: that is ALTER TABLE's DROP COLUMN and DROP CONSTRAINT, which the snail does not write.
    fn take_out(&mut self, catalog: &Catalog, taken: &[crate::cascade::Taken]) -> Result<()> {
        use crate::deparse::depend as d;
        use crate::oids::{full, Object as O};
        let whole = |class: u32, oid: u32| {
            taken
                .iter()
                .any(|t| t.addr.class == class && t.addr.oid == oid && t.addr.sub == 0)
        };
        let of_table = |name: &str| catalog.oids.of(&O::Class(name.to_string())).unwrap_or(0);
        let mut tables: Vec<String> = Vec::new();
        let mut sequences: Vec<String> = Vec::new();
        let mut views: Vec<String> = Vec::new();
        let mut types: Vec<String> = Vec::new();
        let mut functions: Vec<(String, String)> = Vec::new();
        let mut collations: Vec<String> = Vec::new();
        let mut schemas: Vec<String> = Vec::new();
        // The parts of objects left standing: (table or domain, name).
        let mut defaults: Vec<(String, String)> = Vec::new();
        let mut constraints: Vec<(String, String)> = Vec::new();
        let mut indexes: Vec<(String, String)> = Vec::new();
        for t in taken {
            let Some(o) = catalog.oids.object(t.addr.oid) else {
                continue;
            };
            match (t.addr.class, o) {
                (d::NAMESPACE, O::Namespace(n)) => schemas.push(n.clone()),
                (d::CLASS, O::Class(n)) if t.addr.sub != 0 => {
                    if t.part_of_whole() || whole(d::CLASS, t.addr.oid) {
                        continue;
                    }
                    let composite = self.types.iter().any(|x| {
                        full(&x.name) == *n && matches!(x.kind, TypeKind::Composite { .. })
                    });
                    let what = match self.views.iter().any(|v| full(&v.name) == *n) {
                        true => "view",
                        false if composite => "composite type",
                        false => "table",
                    };
                    let column = crate::pgcatalog::file_rows(catalog, "pg_attribute")
                        .iter()
                        .find(|r| {
                            r.get("attrelid").cloned().flatten() == Some(t.addr.oid.to_string())
                                && r.get("attnum").cloned().flatten()
                                    == Some(t.addr.sub.to_string())
                        })
                        .and_then(|r| r.get("attname").cloned().flatten())
                        .unwrap_or_default();
                    let by = match composite {
                        true => "an attribute is dropped by ALTER TYPE … DROP ATTRIBUTE",
                        false => "a column is dropped by ALTER TABLE … DROP COLUMN",
                    };
                    return Err(Error::new(format!(
                        "the drop reaches column {column} of {what} {n}{REACHED}{by}, which the \
                         snail does not write"
                    )));
                }
                (d::CLASS, O::Class(n)) => {
                    if self.tables.iter().any(|x| full(&x.name) == *n) {
                        tables.push(n.clone());
                    } else if self.sequences.iter().any(|x| full(&x.name) == *n) {
                        sequences.push(n.clone());
                    } else if self.views.iter().any(|x| full(&x.name) == *n) {
                        views.push(n.clone());
                    } else if let Some(x) = self.tables.iter().find(|x| {
                        let (s, own) = crate::oids::parts(n);
                        schema_of(&x.name) == s && x.indexes.iter().any(|k| k.name == own)
                    }) {
                        // An index of a table: with it where the table goes; a key's or an
                        // EXCLUDE's goes with its constraint.
                        let own = crate::oids::parts(n).1.to_string();
                        let k = x.indexes.iter().find(|k| k.name == own).unwrap();
                        if k.exclusion.is_none() && !whole(d::CLASS, of_table(&full(&x.name))) {
                            indexes.push((x.name.clone(), own));
                        }
                    }
                }
                (d::TYPE, O::Type(n)) => {
                    let declared = self.types.iter().any(|x| full(&x.name) == *n)
                        || self.domains.iter().any(|x| full(&x.name) == *n);
                    if declared {
                        types.push(n.clone());
                    }
                }
                (d::PROC, O::Proc(n, args)) => {
                    if self.functions.iter().any(|f| full(&f.name) == *n) {
                        functions.push((n.clone(), args.clone()));
                    }
                }
                (d::COLLATION, O::Collation(n)) => collations.push(n.clone()),
                (d::CONSTRAINT, O::Constraint(on, n)) => {
                    if let Some(x) = self.tables.iter().find(|x| full(&x.name) == *on) {
                        if whole(d::CLASS, of_table(on)) {
                            continue;
                        }
                        let keyed = x.primary.iter().chain(&x.uniques).any(|k| k.name == *n);
                        if keyed {
                            return Err(Error::new(format!(
                                "the drop reaches constraint {n} on table {on}, which it leaves: a \
                                 key is dropped by ALTER TABLE … DROP CONSTRAINT, which the snail \
                                 does not write"
                            )));
                        }
                        // An EXCLUDE goes with its index.
                        if x.indexes.iter().any(|k| k.name == *n && k.exclusion.is_some()) {
                            indexes.push((x.name.clone(), n.clone()));
                            continue;
                        }
                        constraints.push((x.name.clone(), n.clone()));
                    } else if let Some(x) = self.domains.iter().find(|x| full(&x.name) == *on) {
                        let oid = catalog.oids.of(&O::Type(full(&x.name))).unwrap_or(0);
                        if !whole(d::TYPE, oid) {
                            constraints.push((x.name.clone(), n.clone()));
                        }
                    }
                }
                (d::ATTRDEF, O::Default(on, c)) if !whole(d::CLASS, of_table(on)) => {
                    if let Some(x) = self.tables.iter().find(|x| full(&x.name) == *on) {
                        defaults.push((x.name.clone(), c.clone()));
                    }
                }
                _ => {}
            }
        }
        // The names of constraints that go, each taken back once from those a schema holds.
        let mut gone: Vec<String> = Vec::new();
        let mut removed_at: Vec<usize> = Vec::new();
        for (i, x) in self.tables.iter().enumerate() {
            if tables.contains(&full(&x.name)) {
                removed_at.push(i);
                let s = schema_of(&x.name);
                gone.extend(x.checks.iter().map(|k| qualified(s, &k.name)));
                gone.extend(x.foreign_keys.iter().map(|k| qualified(s, &k.name)));
            }
        }
        for x in &self.domains {
            if types.contains(&full(&x.name)) {
                let s = schema_of(&x.name);
                gone.extend(x.checks.iter().map(|k| qualified(s, &k.name)));
                gone.extend(x.not_null_name.iter().map(|k| qualified(s, k)));
            }
        }
        for (on, n) in &constraints {
            gone.push(qualified(schema_of(on), n));
            if let Some(x) = self.table_mut(on) {
                x.checks.retain(|k| k.name != *n);
                x.foreign_keys.retain(|k| k.name != *n);
            } else if let Some(x) = self.domains.iter_mut().find(|x| x.name == *on) {
                x.checks.retain(|k| k.name != *n);
                if x.not_null_name.as_deref() == Some(n.as_str()) {
                    x.not_null = false;
                    x.not_null_name = None;
                }
            }
        }
        for (on, c) in &defaults {
            if let Some(col) = self
                .table_mut(on)
                .and_then(|x| x.columns.iter_mut().find(|y| y.name == *c))
            {
                col.default = None;
            }
        }
        for (on, k) in &indexes {
            if let Some(x) = self.table_mut(on) {
                x.indexes.retain(|y| y.name != *k);
                if x.clustered.as_ref() == Some(k) {
                    x.clustered = None;
                }
            }
        }
        self.strands_at.retain(|(_, on, k)| {
            !tables.contains(&full(on)) && !indexes.iter().any(|(t, i)| t == on && i == k)
        });
        for s in &mut self.strands_at {
            s.0 -= removed_at.iter().filter(|&&r| r < s.0).count();
        }
        self.tables.retain(|x| !tables.contains(&full(&x.name)));
        self.sequences.retain(|x| !sequences.contains(&full(&x.name)));
        self.views.retain(|x| !views.contains(&full(&x.name)));
        self.types.retain(|x| !types.contains(&full(&x.name)));
        self.domains.retain(|x| !types.contains(&full(&x.name)));
        self.functions.retain(|f| {
            let args: Vec<String> = f.args.iter().map(|t| t.pg_name()).collect();
            !functions.contains(&(full(&f.name), args.join(",")))
        });
        self.collations.retain(|x| !collations.contains(&full(&x.name)));
        self.schemas.retain(|x| !schemas.contains(x));
        for q in gone {
            if let Some(i) = self.check_names.iter().position(|x| *x == q) {
                self.check_names.remove(i);
            }
        }
        self.recount();
        Ok(())
    }

    /// The counts and places `relation`, `constraint` and `table` read, made again from what the
    /// draft holds, once a DROP has taken objects out of it.
    fn recount(&mut self) {
        self.table_at.clear();
        self.relations.clear();
        self.key_constraints.clear();
        for t in std::mem::take(&mut self.tables) {
            self.push_table(t);
        }
        for s in &self.sequences {
            count_in(
                &mut self.relations,
                in_schema(schema_of(&s.name), bare(&s.name)),
            );
        }
        for v in &self.views {
            count_in(
                &mut self.relations,
                in_schema(schema_of(&v.name), bare(&v.name)),
            );
        }
        self.check_named.clear();
        for q in &self.check_names {
            count_in(&mut self.check_named, q.clone());
        }
    }

    /// The table and column a sequence's OWNED BY names, checked; or what it was owned by before.
    fn owner(
        &self,
        sequence: &str,
        options: &[SequenceOption],
        before: Option<(String, String)>,
    ) -> Result<Option<(String, String)>> {
        let Some(owned) = options.iter().rev().find_map(|o| match o {
            SequenceOption::OwnedBy(x) => Some(x.clone()),
            _ => None,
        }) else {
            return Ok(before);
        };
        let Some((table, column)) = owned else {
            return Ok(None);
        };
        let t = self
            .table(&table)
            .ok_or_else(|| Error::new(format!("relation \"{table}\" does not exist")))?;
        if !t.columns.iter().any(|c| c.name == column) {
            return Err(Error::new(format!(
                "column \"{column}\" of relation \"{table}\" does not exist"
            )));
        }
        if schema_of(&table) != schema_of(sequence) {
            return Err(Error::new(format!(
                "{sequence}: sequence must be in same schema as table it is linked to"
            )));
        }
        Ok(Some((table, column)))
    }

    /// ALTER TABLE … ADD [COLUMN] a column, ADD [CONSTRAINT n] CHECK (…) [NOT VALID], ADD [CONSTRAINT
    /// n] FOREIGN KEY …,
    /// VALIDATE CONSTRAINT, and ALTER [COLUMN] … SET DEFAULT or DROP DEFAULT; CLUSTER ON is read
    /// before. Any other form is refused by name.
    fn alter_table(&mut self, tokens: &[Token], object: &str) -> Result<Vec<String>> {
        self.altered(tokens, object)
    }

    /// ALTER TABLE … ADD [COLUMN] [IF NOT EXISTS] and a column's definition: read as CREATE TABLE
    /// reads one, over a copy of the catalog so far, and put at the table's end; a constraint, a
    /// serial, an identity or a generation written with it refused by name. The table's rows decide whether it is
    /// laid: a table that holds rows keeps its columns as written (the writer's refusal).
    fn add_column(
        &mut self,
        name: &str,
        definition: &str,
        if_not_exists: bool,
        mut notices: Vec<String>,
    ) -> Result<Vec<String>> {
        let relation = bare(name).to_string();
        let what = format!("ALTER TABLE {name} ADD COLUMN {definition}");
        let serial = definition.split_whitespace().any(|w| {
            matches!(
                w.to_ascii_lowercase().as_str(),
                "serial" | "serial2" | "serial4" | "serial8" | "smallserial" | "bigserial"
                    | "generated"
            )
        });
        if serial {
            return Err(Error::new(format!(
                "{what}: a constraint, a serial, an identity or a generation added with a column \
                 is not read"
            )));
        }
        let made = format!("{}.\u{7}added", schema_of(name));
        let text = format!(
            "CREATE TABLE \"{}\".\"\u{7}added\" ({definition})",
            schema_of(name)
        );
        let tokens = Tokenizer::new(&PostgreSqlDialect {}, &text).tokenize()?;
        let Some((kept, extras, fks, temporal)) = create_table_extras(&tokens)? else {
            return Err(Error::new(format!("{what}: its column is not read")));
        };
        let mut copy = self.clone();
        copy.create_table(&kept, extras, fks, &temporal)?;
        let t = copy
            .table(&made)
            .ok_or_else(|| Error::new(format!("{what}: its column is not read")))?;
        let [col] = t.columns.as_slice() else {
            return Err(Error::new(format!("{what}: one column is added at a time")));
        };
        let beside = t.primary.is_some()
            || !t.uniques.is_empty()
            || !t.checks.is_empty()
            || !t.indexes.is_empty()
            || !t.foreign_keys.is_empty()
            || copy.sequences.len() != self.sequences.len()
            || col.identity.is_some();
        if beside {
            return Err(Error::new(format!(
                "{what}: a constraint, a serial, an identity or a generation added with a column \
                 is not read"
            )));
        }
        let col = col.clone();
        let table = self.table_mut(name).expect("the table altered");
        if table.columns.iter().any(|x| x.name == col.name) {
            if if_not_exists {
                notices.push(format!(
                    "column \"{}\" of relation \"{relation}\" already exists, skipping",
                    col.name
                ));
                return Ok(notices);
            }
            return Err(Error::new(format!(
                "column \"{}\" of relation \"{relation}\" already exists",
                col.name
            )));
        }
        table.columns.push(col);
        Ok(notices)
    }

    /// The forms of ALTER TABLE, as `alter_table` names them.
    fn altered(&mut self, tokens: &[Token], object: &str) -> Result<Vec<String>> {
        let mut c = Cursor::new(tokens);
        c.kws(&["ALTER", "TABLE"]);
        let if_exists = c.kws(&["IF", "EXISTS"]);
        c.kw("ONLY");
        let name = c.name()?;
        let schema = schema_of(&name).to_string();
        let mut notices = Vec::new();
        if self.table(&name).is_none() {
            if if_exists {
                notices.push(format!("relation \"{}\" does not exist, skipping", bare(&name)));
                return Ok(notices);
            }
            return Err(Error::new(format!("relation \"{name}\" does not exist")));
        }
        if c.kws(&["VALIDATE", "CONSTRAINT"]) {
            let n = c.ident()?;
            end(&c, &format!("ALTER TABLE {name}"))?;
            let t = self.table_mut(&name).unwrap();
            if let Some(k) = t.foreign_keys.iter_mut().find(|k| k.name == n) {
                if !k.enforced {
                    return Err(Error::new(format!(
                        "cannot validate NOT ENFORCED constraint \"{n}\" of relation \"{}\"",
                        bare(&name)
                    )));
                }
                k.valid = true;
                return Ok(notices);
            }
            let k = t.checks.iter_mut().find(|k| k.name == n).ok_or_else(|| {
                Error::new(format!(
                    "constraint \"{n}\" of relation \"{name}\" does not exist"
                ))
            })?;
            k.valid = true;
            return Ok(notices);
        }
        // A column's default set or dropped: what fills it where a row gives it none, no row held
        // moving.
        if c.kw("ALTER") {
            c.kw("COLUMN");
            let column = c.ident()?;
            let set = if c.kws(&["SET", "DEFAULT"]) {
                Some(c.expr_until(&[])?)
            } else if c.kws(&["DROP", "DEFAULT"]) {
                None
            } else {
                return Err(Error::new(format!(
                    "ALTER TABLE {object}: of ALTER COLUMN, only SET DEFAULT and DROP DEFAULT are \
                     read"
                )));
            };
            end(&c, &format!("ALTER TABLE {name}"))?;
            let relation = bare(&name).to_string();
            let t = self.table_mut(&name).unwrap();
            let at = t.columns.iter().position(|x| x.name == column).ok_or_else(|| {
                Error::new(format!(
                    "column \"{column}\" of relation \"{relation}\" does not exist"
                ))
            })?;
            // A constant is cast to the column's type as it is set, as PostgreSQL's parser casts
            // it: refused where the column's input refuses it. A fold's copy of the statement is
            // read by the literal each mark stands for.
            if let (Some(e), Ok(table)) = (set.as_ref().filter(|e| constant(e)), finish_table(t)) {
                let e = crate::deparse::analyze::unmarked_expr(e);
                let e = e.as_ref().or(set.as_ref()).unwrap();
                if let Err(why) = crate::read::statement::datum(e, &table, at) {
                    if why.sqlstate().is_some_and(|code| code.starts_with("22")) {
                        return Err(why);
                    }
                }
            }
            let col = &mut t.columns[at];
            if col.identity.is_some() {
                return Err(Error::new(format!(
                    "column \"{column}\" of relation \"{relation}\" is an identity column"
                )));
            }
            if col.generated.is_some() {
                return Err(Error::new(format!(
                    "column \"{column}\" of relation \"{relation}\" is a generated column"
                )));
            }
            col.default = set;
            return Ok(notices);
        }
        if !c.kw("ADD") {
            return Err(Error::new(format!(
                "ALTER TABLE {object}: only ADD COLUMN, ADD CHECK, ADD FOREIGN KEY, VALIDATE \
                 CONSTRAINT, CLUSTER ON, and ALTER COLUMN's SET DEFAULT and DROP DEFAULT are read"
            )));
        }
        // Columns added, each with COLUMN or with no constraint's word after ADD, one or several a
        // comma apart.
        let constraint = |c: &Cursor| {
            ["CONSTRAINT", "CHECK", "FOREIGN", "PRIMARY", "UNIQUE", "EXCLUDE"]
                .iter()
                .any(|k| c.is(k))
        };
        if c.is("COLUMN") || !constraint(&c) {
            c.i -= 1;
            let mut actions = Vec::new();
            while !c.done() {
                actions.push(c.item());
            }
            for action in actions {
                let mut a = Cursor { t: action, i: 0 };
                if !a.kw("ADD") || !(a.kw("COLUMN") || !constraint(&a)) {
                    return Err(Error::new(format!(
                        "ALTER TABLE {object}: an ADD COLUMN beside another action is not read"
                    )));
                }
                let if_not_exists = a.kws(&["IF", "NOT", "EXISTS"]);
                notices = self.add_column(&name, &a.rest(), if_not_exists, notices)?;
            }
            return Ok(notices);
        }
        let at = c.i;
        if let Some(r) = table_key_reference(&mut c, &format!("ALTER TABLE {name}"))? {
            end(&c, &format!("ALTER TABLE {name} ADD FOREIGN KEY"))?;
            let k = self.reference(&name, r, false)?;
            parting(&name, &self.table(&name).unwrap().foreign_keys, &k)?;
            self.push_check_name(qualified(&schema, &k.name));
            self.table_mut(&name).unwrap().foreign_keys.push(k);
            return Ok(notices);
        }
        c.i = at;
        let given = if c.kw("CONSTRAINT") {
            Some(c.ident()?)
        } else {
            None
        };
        if !c.kw("CHECK") {
            return Err(Error::new(format!(
                "ALTER TABLE {object} ADD {}: only a CHECK or a FOREIGN KEY is added",
                c.rest()
            )));
        }
        let expr = expression(&c.group()?)?;
        let mut valid = true;
        let mut enforced = true;
        while !c.done() {
            if c.kws(&["NOT", "VALID"]) {
                valid = false;
            } else if c.kws(&["NOT", "ENFORCED"]) {
                enforced = false;
            } else if c.kw("ENFORCED") {
            } else if c.kws(&["NO", "INHERIT"]) {
                notices.push(format!(
                    "{name}: NO INHERIT is read, and does nothing: no table of a snail inherits"
                ));
            } else {
                return Err(Error::new(format!(
                    "ALTER TABLE {name} ADD CHECK: {} is not read",
                    c.rest()
                )));
            }
        }
        let t = self.table(&name).unwrap();
        for r in references(&expr) {
            if !t.columns.iter().any(|x| x.name == r) {
                return Err(Error::new(format!("column \"{r}\" does not exist")));
            }
        }
        let n = match given {
            Some(n) => {
                if t.checks.iter().any(|k| k.name == n) || self.constraint(&schema, &n) {
                    return Err(Error::new(format!(
                        "constraint \"{n}\" for relation \"{}\" already exists", bare(&name)
                    )));
                }
                n
            }
            None => {
                let refs = references(&expr);
                let col = match refs.as_slice() {
                    [one] => Some(one.as_str()),
                    _ => None,
                };
                choose(bare(&name), col, "check", &|n| self.constraint(&schema, n))
            }
        };
        self.push_check_name(qualified(&schema, &n));
        self.table_mut(&name).unwrap().checks.push(Check {
            name: n,
            expr,
            enforced,
            valid: valid && enforced,
        });
        Ok(notices)
    }

    fn create_table(
        &mut self,
        kept: &[Token],
        mut extras: Vec<(String, ColumnExtras)>,
        fks: Vec<KeyReference>,
        without_overlaps: &[usize],
    ) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        let st = Parser::new(&crate::grammar::POSTGRES)
            .with_tokens(kept.to_vec())
            .parse_statement()?;
        let Statement::CreateTable(t) = &st else {
            return Err(Error::new(
                "CREATE TABLE: sqlparser reads another statement",
            ));
        };
        let name = object_name(&t.name);
        let what = format!("CREATE TABLE {name}");
        read_as_written(kept, &st, &what)?;
        let plain = Statement::CreateTable(
            CreateTableBuilder::new(t.name.clone())
                .columns(t.columns.clone())
                .constraints(t.constraints.clone())
                .if_not_exists(t.if_not_exists)
                .build(),
        );
        if plain.to_string() != st.to_string() {
            let spoken = |s: &Statement| -> Result<Vec<Token>> {
                Ok(crate::read::image::tokens(&s.to_string())?
                    .into_iter()
                    .map(|t| t.token)
                    .collect())
            };
            let (a, b) = (spoken(&st)?, spoken(&plain)?);
            let (a, b) = (Cursor::new(&a).t, Cursor::new(&b).t);
            let at = a
                .iter()
                .zip(&b)
                .position(|(x, y)| x != y)
                .unwrap_or(b.len());
            return Err(Error::new(format!(
                "{what}: {} is not read: only a table's columns, its constraints and IF NOT \
                 EXISTS are",
                text(&a[at..(at + 8).min(a.len())])
            )));
        }
        let schema = schema_of(&name).to_string();
        if self.table(&name).is_some()
            || self.relation(&schema, bare(&name))
            || self.composite_named(&name)
        {
            if t.if_not_exists {
                notices.push(format!("relation \"{}\" already exists, skipping", bare(&name)));
                return Ok(notices);
            }
            return Err(Error::new(format!("relation \"{}\" already exists", bare(&name))));
        }
        if self.type_named(&name) {
            return Err(row_type_taken(&name));
        }
        let mut columns: Vec<Column> = Vec::new();
        let mut keys: Vec<KeySpec> = Vec::new();
        // Each CHECK: its name where written, its expression, NOT ENFORCED, and the column it is
        // written on.
        let mut checks: Vec<(Option<String>, Expr, bool)> = Vec::new();
        let mut excludes: Vec<sqlparser::ast::ExcludeConstraint> = Vec::new();
        let mut serials: Vec<(String, SequenceType)> = Vec::new();
        let mut identities: Vec<(String, SequenceType, bool, Vec<SequenceOption>)> = Vec::new();
        for c in &t.columns {
            let cname = ident(&c.name);
            let cwhat = format!("{name}.{cname}");
            if columns.iter().any(|x| x.name == cname) {
                return Err(Error::new(format!(
                    "column \"{cname}\" specified more than once"
                )));
            }
            if crate::oids::SYSTEM_COLUMNS.contains(&cname.as_str()) {
                return Err(Error::new(format!(
                    "column name \"{cname}\" conflicts with a system column name"
                )));
            }
            let x = match extras.iter().position(|(n, _)| *n == cname) {
                Some(i) => extras.remove(i).1,
                None => ColumnExtras::default(),
            };
            let serial = match &c.data_type {
                DataType::Custom(n, args) if args.is_empty() => match object_name(n).as_str() {
                    "serial" | "serial4" => Some(SequenceType::Int4),
                    "smallserial" | "serial2" => Some(SequenceType::Int2),
                    "bigserial" | "serial8" => Some(SequenceType::Int8),
                    _ => None,
                },
                _ => None,
            };
            let (ty, domain) = match serial {
                Some(s) => (s.column(), None),
                None => self.held(&c.data_type, &cwhat)?,
            };
            let mut col = Column {
                name: cname.clone(),
                ty,
                not_null: false,
                default: None,
                generated: None,
                identity: None,
                collation: None,
                domain,
                library: None,
                dims: crate::pgcatalog::array_dims(&c.data_type),
            };
            let mut nullness: Option<bool> = None;
            for o in &c.options {
                let given = o.name.as_ref().map(ident);
                match &o.option {
                    ColumnOption::NotNull | ColumnOption::Null => {
                        let not = matches!(o.option, ColumnOption::NotNull);
                        if nullness.is_some_and(|n| n != not) {
                            return Err(Error::new(format!(
                                "conflicting NULL/NOT NULL declarations for column \"{cname}\" of table \"{name}\""
                            )));
                        }
                        nullness = Some(not);
                        col.not_null |= not;
                    }
                    ColumnOption::Default(e) => {
                        if col.default.is_some() {
                            return Err(Error::new(format!(
                                "multiple default values specified for column \"{cname}\" of table \"{name}\""
                            )));
                        }
                        col.default = Some(crate::grammar::expression(e.clone()));
                    }
                    ColumnOption::PrimaryKey(p) => keys.push(KeySpec {
                        name: given.or_else(|| p.name.as_ref().map(ident)),
                        primary: true,
                        columns: vec![cname.clone()],
                        include: Vec::new(),
                        nulls_distinct: true,
                        deferrable: deferrable(p.characteristics.as_ref(), &cwhat)?,
                        without_overlaps: false,
                    }),
                    ColumnOption::Unique(u) => keys.push(KeySpec {
                        name: given.or_else(|| u.name.as_ref().map(ident)),
                        primary: false,
                        columns: vec![cname.clone()],
                        include: Vec::new(),
                        nulls_distinct: x.unique_nulls_distinct.unwrap_or(true),
                        deferrable: deferrable(u.characteristics.as_ref(), &cwhat)?,
                        without_overlaps: false,
                    }),
                    ColumnOption::Check(k) => {
                        if k.no_inherit {
                            notices.push(format!(
                                "{cwhat}: NO INHERIT is read, and does nothing: no table of a \
                                 snail inherits"
                            ));
                        }
                        checks.push((
                            given.or_else(|| k.name.as_ref().map(ident)),
                            crate::grammar::expression((*k.expr).clone()),
                            k.enforced != Some(false),
                        ));
                    }
                    ColumnOption::Collation(n) => {
                        let collation = object_name(n);
                        if !collatable(ty) {
                            return Err(Error::new(format!(
                                "{cwhat}: COLLATE {collation} on a column that is not text"
                            )));
                        }
                        if !self.kept {
                            match crate::catalog::collation_casing(&collation, &self.collations) {
                                Ok(_) => {}
                                Err(_) if !cfg!(feature = "collations") => {
                                    return Err(Error::new(format!(
                                        "{cwhat}: COLLATE {collation}: {}",
                                        crate::catalog::NOT_HELD
                                    )))
                                }
                                Err(e) => return Err(Error::new(format!("{cwhat}: {e}"))),
                            }
                        }
                        col.library = crate::catalog::library_spec(&collation, &self.collations);
                        col.collation = Some(collation);
                    }
                    ColumnOption::ForeignKey(_) => {
                        return Err(Error::new(format!(
                            "{cwhat}: a FOREIGN KEY is not held yet"
                        )))
                    }
                    other => {
                        return Err(Error::new(format!(
                            "{cwhat}: the option {other} is not read"
                        )))
                    }
                }
            }
            if let Some(s) = serial {
                if col.default.is_some() {
                    return Err(Error::new(format!(
                        "multiple default values specified for column \"{cname}\" of table \"{name}\""
                    )));
                }
                col.not_null = true;
                serials.push((cname.clone(), s));
            }
            if let Some((always, options)) = x.identity {
                if col.default.is_some() {
                    return Err(Error::new(format!(
                        "both default and identity specified for column \"{cname}\" of table \"{name}\""
                    )));
                }
                let s = match ty {
                    ColumnType::Int2 => SequenceType::Int2,
                    ColumnType::Int4 => SequenceType::Int4,
                    ColumnType::Int8 => SequenceType::Int8,
                    _ => {
                        return Err(Error::new(format!(
                            "{cwhat}: identity column type must be smallint, integer, or bigint"
                        )))
                    }
                };
                col.not_null = true;
                identities.push((cname.clone(), s, always, options));
            }
            if let Some(g) = x.generated {
                if col.default.is_some() {
                    return Err(Error::new(format!(
                        "both default and generation expression specified for column \"{cname}\" of table \"{name}\""
                    )));
                }
                if !identities.iter().all(|(n, ..)| *n != cname) {
                    return Err(Error::new(format!(
                        "both identity and generation expression specified for column \"{cname}\" of table \"{name}\""
                    )));
                }
                col.generated = Some(g);
            }
            columns.push(col);
        }
        // The table's PRIMARY KEYs and UNIQUEs, counted in the order written.
        let mut table_keys = 0;
        let mut overlapping = || {
            table_keys += 1;
            without_overlaps.contains(&(table_keys - 1))
        };
        for k in &t.constraints {
            match k {
                TableConstraint::PrimaryKey(p) => keys.push(KeySpec {
                    name: p.name.as_ref().map(ident),
                    primary: true,
                    columns: key_columns(&p.columns, &name)?,
                    include: p.include.iter().map(ident).collect(),
                    nulls_distinct: true,
                    deferrable: deferrable(p.characteristics.as_ref(), &name)?,
                    without_overlaps: overlapping(),
                }),
                TableConstraint::Unique(u) => {
                    if u.index_name.is_some()
                        || u.index_type.is_some()
                        || !u.index_options.is_empty()
                    {
                        return Err(Error::new(format!(
                            "{name}: the constraint {k} is not read"
                        )));
                    }
                    keys.push(KeySpec {
                        name: u.name.as_ref().map(ident),
                        primary: false,
                        columns: key_columns(&u.columns, &name)?,
                        include: u.include.iter().map(ident).collect(),
                        nulls_distinct: u.nulls_distinct != NullsDistinctOption::NotDistinct,
                        deferrable: deferrable(u.characteristics.as_ref(), &name)?,
                        without_overlaps: overlapping(),
                    })
                }
                TableConstraint::Check(c) => {
                    if c.no_inherit {
                        notices.push(format!(
                            "{name}: NO INHERIT is read, and does nothing: no table of a snail \
                             inherits"
                        ));
                    }
                    checks.push((
                        c.name.as_ref().map(ident),
                        crate::grammar::expression((*c.expr).clone()),
                        c.enforced != Some(false),
                    ))
                }
                TableConstraint::ForeignKey(_) => {
                    return Err(Error::new(format!("{name}: a FOREIGN KEY is not held yet")))
                }
                TableConstraint::Exclude(x) => excludes.push(x.clone()),
                other => {
                    return Err(Error::new(format!(
                        "{name}: the constraint {other} is not read"
                    )))
                }
            }
        }
        if keys.iter().filter(|k| k.primary).count() > 1 {
            return Err(Error::new(format!(
                "multiple primary keys for table \"{name}\" are not allowed"
            )));
        }
        // The draft's own view of the table, so that the columns' defaults and the sequences their
        // serials and identities make are checked against it.
        let names: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        let column_index = |n: &str| names.iter().position(|c| c == n);
        for k in &keys {
            for n in k.columns.iter().chain(&k.include) {
                let Some(i) = column_index(n) else {
                    return Err(Error::new(format!(
                        "column \"{n}\" named in key does not exist"
                    )));
                };
                if columns[i].is_virtual() {
                    return Err(Error::new(format!(
                        "{name}.{n}: a key on a virtual generated column is not supported"
                    )));
                }
            }
            if k.without_overlaps {
                let last = k.columns.last().expect("a key names a column");
                let ty = columns[column_index(last).unwrap()].ty;
                if !crate::spans::is_period(ty) {
                    return Err(Error::new(format!(
                        "column \"{last}\" in WITHOUT OVERLAPS is not a range or multirange type"
                    )));
                }
                if k.columns.len() < 2 {
                    return Err(Error::new(
                        "constraint using WITHOUT OVERLAPS needs at least two columns",
                    ));
                }
            }
        }
        for k in keys.iter().filter(|k| k.primary) {
            for n in &k.columns {
                let i = column_index(n).unwrap();
                columns[i].not_null = true;
            }
        }
        for col in &columns {
            if let Some(g) = &col.generated {
                for r in references(&g.expr) {
                    match columns.iter().find(|c| c.name == r) {
                        None => {
                            return Err(Error::new(format!(
                                "{name}.{}: column \"{r}\" does not exist",
                                col.name
                            )))
                        }
                        Some(c) if c.generated.is_some() => {
                            return Err(Error::new(format!(
                                "{name}.{}: cannot use generated column \"{r}\" in column \
                                 generation expression",
                                col.name
                            )))
                        }
                        Some(_) => {}
                    }
                }
            }
        }
        for (_, e, _) in &checks {
            for r in references(e) {
                if !columns.iter().any(|c| c.name == r) {
                    return Err(Error::new(format!("{name}: column \"{r}\" does not exist")));
                }
            }
        }
        // The table made whole or not at all: where it is refused, what it added is taken back. The
        // sequences, then the defaults that read them.
        let undo = (
            self.tables.len(),
            self.sequences.len(),
            self.check_names.len(),
            self.strands_at.len(),
        );
        let next = &mut *self;
        let made = (move || -> Result<Vec<String>> {
            next.push_table(Declared {
                name: name.clone(),
                columns: columns.clone(),
                primary: None,
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: Vec::new(),
                clustered: None,
                foreign_keys: Vec::new(),
            });
            for (cname, s) in serials {
                let seq = next.column_sequence(&name, &cname, s, &[], false)?;
                let i = columns.iter().position(|c| c.name == cname).unwrap();
                columns[i].default = Some(nextval_expr(&seq));
            }
            for (cname, s, always, options) in identities {
                let seq = next.column_sequence(&name, &cname, s, &options, true)?;
                let i = columns.iter().position(|c| c.name == cname).unwrap();
                columns[i].identity = Some(Identity {
                    always,
                    sequence: seq,
                });
            }
            for col in &columns {
                if let Some(e) = &col.default {
                    next.check_default(e, col.ty, &format!("column \"{}\"", col.name))?;
                }
            }
            // The keys, named as PostgreSQL names them: the primary key first, then each UNIQUE in the
            // order written, one written twice made once.
            keys.sort_by_key(|k| !k.primary);
            let mut made: Vec<Keyed> = Vec::new();
            // Whether each key's name was chosen rather than written.
            let mut chosen: Vec<bool> = Vec::new();
            for k in keys {
                let levels: Vec<Level> = k
                    .columns
                    .iter()
                    .map(|n| Level::Column(column_index(n).unwrap()))
                    .collect();
                let include: Vec<usize> =
                    k.include.iter().map(|n| column_index(n).unwrap()).collect();
                let method = if k.without_overlaps {
                    Method::Gist
                } else {
                    Method::Btree
                };
                if let Some(at) = made.iter().position(|m| {
                    m.levels == levels
                        && m.include == include
                        && m.nulls_distinct == k.nulls_distinct
                        && m.constraint.as_ref().map(|c| c.deferrable) == Some(k.deferrable)
                        && m.method == method
                }) {
                    if let Some(c) = made[at].constraint.as_mut() {
                        c.primary |= k.primary;
                    }
                    // A name written takes the place of one chosen, as PostgreSQL's
                    // transformIndexConstraints gives it to the key it keeps.
                    if let (true, Some(n)) = (chosen[at], &k.name) {
                        if next.relation(&schema, n) || made.iter().any(|m| &m.name == n) {
                            return Err(Error::new(format!("relation \"{n}\" already exists")));
                        }
                        made[at].name = n.clone();
                        chosen[at] = false;
                    }
                    continue;
                }
                let kname = match &k.name {
                    Some(n) => {
                        if next.relation(&schema, n) || made.iter().any(|m| &m.name == n) {
                            return Err(Error::new(format!("relation \"{n}\" already exists")));
                        }
                        n.clone()
                    }
                    None => {
                        // PostgreSQL names a key for its columns and those it includes.
                        let all: Vec<String> =
                            k.columns.iter().chain(&k.include).cloned().collect();
                        let addition = name_addition(&all);
                        choose(
                            bare(&name),
                            if k.primary { None } else { Some(&addition) },
                            if k.primary { "pkey" } else { "key" },
                            &|n| {
                                next.relation(&schema, n)
                                    || next.constraint(&schema, n)
                                    || made.iter().any(|m| m.name == n)
                            },
                        )
                    }
                };
                // A key WITHOUT OVERLAPS is an EXCLUDE on GiST: `=` on each column but its last, `&&`
                // on its last, as PostgreSQL's ComputeIndexAttrs finds them in each column's operator
                // class.
                let (opclasses, exclusion) = if k.without_overlaps {
                    let mut opclasses = Vec::new();
                    let mut operators = Vec::new();
                    for (i, l) in levels.iter().enumerate() {
                        let ty = columns[l.column()].ty;
                        let op = crate::methods::opclass(Method::Gist, ty, None, next.release)?;
                        let operator = if i + 1 == levels.len() { "&&" } else { "=" };
                        if crate::methods::exclusion_operator(Method::Gist, &op, ty, operator)
                            .is_err()
                        {
                            return Err(Error::new(format!(
                                "could not identify an equality operator for type {}",
                                crate::methods::bare_name(ty)
                            )));
                        }
                        if !crate::methods::held(Method::Gist, &op) {
                            return Err(Error::new(format!(
                                "{name}: a key WITHOUT OVERLAPS on {}: USING gist with the operator \
                                 class {op} is not held yet",
                                columns[l.column()].name
                            )));
                        }
                        opclasses.push(op);
                        operators.push(operator.to_string());
                    }
                    let exclusion = crate::catalog::Exclusion {
                        operators,
                        deferrable: k.deferrable,
                    };
                    (opclasses, Some(exclusion))
                } else {
                    let opclasses = levels
                        .iter()
                        .map(|l| columns[l.column()].ty.default_opclass().to_string())
                        .collect();
                    (opclasses, None)
                };
                chosen.push(k.name.is_none());
                made.push(Keyed {
                    name: kname,
                    method,
                    predicate: None,
                    predicate_columns: Vec::new(),
                    // A key WITHOUT OVERLAPS refuses what overlaps, the same key among it, by its
                    // EXCLUDE.
                    unique: exclusion.is_none(),
                    exclusion,
                    nulls_first: vec![false; levels.len()],
                    desc: vec![false; levels.len()],
                    keys: k.columns.clone(),
                    opclasses,
                    levels,
                    include,
                    nulls_distinct: k.nulls_distinct,
                    options: Vec::new(),
                    params: Vec::new(),
                    constraint: Some(KeyConstraint {
                        primary: k.primary,
                        deferrable: k.deferrable,
                    }),
                });
            }
            let mut used: Vec<String> = Vec::new();
            let mut table_checks = Vec::new();
            for (given, expr, enforced) in checks {
                let n = match given {
                    Some(n) => {
                        if used.contains(&n) || table_checks.iter().any(|c: &Check| c.name == n) {
                            return Err(Error::new(format!(
                                "constraint \"{n}\" for relation \"{}\" already exists", bare(&name)
                            )));
                        }
                        n
                    }
                    None => {
                        let refs = references(&expr);
                        let col = match refs.as_slice() {
                            [one] => Some(one.as_str()),
                            _ => None,
                        };
                        choose(bare(&name), col, "check", &|n| {
                            next.constraint(&schema, n)
                                || made.iter().any(|m| m.name == n)
                                || used.iter().any(|u| u == n)
                        })
                    }
                };
                used.push(n.clone());
                // PostgreSQL marks a CHECK NOT ENFORCED never validated.
                table_checks.push(Check {
                    name: n,
                    expr,
                    enforced,
                    valid: enforced,
                });
            }
            for n in &used {
                next.push_check_name(qualified(&schema, n));
            }
            let d = next.tables.last_mut().unwrap();
            d.columns = columns;
            let mut made = made.into_iter();
            let first = made.next();
            match first {
                Some(k) if k.constraint.as_ref().is_some_and(|c| c.primary) => {
                    d.primary = Some(k);
                    d.uniques = made.collect();
                }
                Some(k) => {
                    d.uniques = std::iter::once(k).chain(made).collect();
                }
                None => {}
            }
            for k in d.primary.iter().chain(d.uniques.iter()) {
                let named = in_schema(schema_of(&d.name), &k.name);
                count_in(&mut next.relations, named.clone());
                count_in(&mut next.key_constraints, named);
            }
            d.checks = table_checks;
            for x in excludes {
                notices.extend(next.exclude(&name, &x)?);
            }
            // The foreign keys, named as PostgreSQL names them, in the order written: once the table's
            // own keys and EXCLUDEs are made, so that it may reference itself. Each is valid, the table
            // being empty.
            for r in fks {
                let k = next.reference(&name, r, true)?;
                parting(&name, &next.table(&name).unwrap().foreign_keys, &k)?;
                next.push_check_name(qualified(&schema, &k.name));
                next.table_mut(&name).unwrap().foreign_keys.push(k);
            }
            Ok(notices)
        })();
        if made.is_err() {
            self.take_back(undo.0, undo.1, undo.2);
            self.strands_at.truncate(undo.3);
        }
        made
    }

    /// An EXCLUDE constraint of a table: the index of its method that stands for it, read as its
    /// CREATE INDEX would be, each element's operator commutative and of its operator class.
    fn exclude(
        &mut self,
        table: &str,
        x: &sqlparser::ast::ExcludeConstraint,
    ) -> Result<Vec<String>> {
        let schema = schema_of(table).to_string();
        let method = match &x.index_method {
            None => Method::Btree,
            Some(m) => {
                let m = ident(m).to_lowercase();
                Method::of_name(&m)
                    .ok_or_else(|| Error::new(format!("access method \"{m}\" does not exist")))?
            }
        };
        let am = method.name();
        if matches!(method, Method::Gin | Method::Brin) {
            return Err(Error::new(format!(
                "access method \"{am}\" does not support exclusion constraints"
            )));
        }
        let keys: Vec<Expr> = x
            .elements
            .iter()
            .map(|e| e.column.column.expr.clone())
            .collect();
        let name = match &x.name {
            Some(n) => ident(n),
            None => {
                let addition = name_addition(&key_names(&keys));
                choose(bare(table), Some(&addition), "excl", &|n| {
                    self.relation(&schema, n) || self.constraint(&schema, n)
                })
            }
        };
        let deferrable = deferrable(x.characteristics.as_ref(), &name)?;
        let mut text = format!(
            "CREATE INDEX \"{}\" ON {table} USING {am} ({})",
            name.replace('"', "\"\""),
            x.elements
                .iter()
                .map(|e| e.column.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if !x.include.is_empty() {
            let include: Vec<String> = x.include.iter().map(|i| i.to_string()).collect();
            text.push_str(&format!(" INCLUDE ({})", include.join(", ")));
        }
        if let Some(p) = &x.where_clause {
            text.push_str(&format!(" WHERE ({p})"));
        }
        let tokens = statements(&text)?.into_iter().next().unwrap_or_default();
        let notices = self.create_index(&tokens)?;
        let t = self.table_mut(table).unwrap();
        let columns = t.columns.clone();
        let k = t.indexes.last_mut().unwrap();
        let mut operators = Vec::new();
        for (i, e) in x.elements.iter().enumerate() {
            let op = e.operator.to_string();
            let ty = columns[k.levels[i].column()].ty;
            crate::methods::exclusion_operator(method, &k.opclasses[i], ty, &op)?;
            operators.push(op);
        }
        k.exclusion = Some(crate::catalog::Exclusion {
            operators,
            deferrable,
        });
        Ok(notices)
    }

    /// A foreign key of the table `child` as written, its columns and its parent's key found: the
    /// parent a table the draft holds, the child itself among them. `made` where the child is made
    /// by the statement, so that NOT VALID has nothing to pass over.
    fn reference(&self, child: &str, r: KeyReference, made: bool) -> Result<ForeignKey> {
        let t = self.table(child).expect("the child the draft holds");
        let schema = schema_of(child);
        let column_of = |d: &Declared, n: &str| -> Result<usize> {
            d.columns.iter().position(|c| c.name == n).ok_or_else(|| {
                Error::new(format!(
                    "column \"{n}\" referenced in foreign key constraint does not exist"
                ))
            })
        };
        let columns: Vec<usize> = r
            .columns
            .iter()
            .map(|n| column_of(t, n))
            .collect::<Result<_>>()?;
        let parent = self.table(&r.parent).ok_or_else(|| match r.parent.rsplit_once('.') {
            // As PostgreSQL's lookup of the referenced table refuses a schema first.
            Some((s, _)) if !self.kept && !self.schema_stands(s) => {
                Error::new(format!("schema \"{s}\" does not exist"))
            }
            _ => Error::new(format!("relation \"{}\" does not exist", r.parent)),
        })?;
        let pname = bare(&parent.name);
        let with_period = r.period || r.parent_period;
        let referenced_only = || {
            Error::new(
                "foreign key uses PERIOD on the referenced table but not the referencing table",
            )
        };
        if with_period && !r.period {
            return Err(referenced_only());
        }
        // The key referenced: the primary key where no columns are written, else the first unique
        // key on those columns, as PostgreSQL's transformFkeyCheckAttrs finds it; with PERIOD, the
        // first EXCLUDE, a key WITHOUT OVERLAPS among them, whose last column is the PERIOD.
        let (key, parent_columns) = match &r.parent_columns {
            None => {
                let p = parent.primary.as_ref().ok_or_else(|| {
                    Error::new(format!(
                        "there is no primary key for referenced table \"{pname}\""
                    ))
                })?;
                if p.constraint.as_ref().is_some_and(|k| k.deferrable) {
                    return Err(Error::new(format!(
                        "cannot use a deferrable primary key for referenced table \"{pname}\""
                    )));
                }
                if p.exclusion.is_some() && !r.period {
                    return Err(referenced_only());
                }
                (p, p.levels.iter().map(|l| l.column()).collect::<Vec<_>>())
            }
            Some(names) => {
                let wanted: Vec<usize> = names
                    .iter()
                    .map(|n| column_of(parent, n))
                    .collect::<Result<_>>()?;
                if dedup(wanted.iter().copied()).len() != wanted.len() {
                    return Err(Error::new(
                        "foreign key referenced-columns list must not contain duplicates",
                    ));
                }
                if with_period && !r.parent_period {
                    return Err(Error::new(
                        "foreign key uses PERIOD on the referencing table but not the referenced \
                         table",
                    ));
                }
                let mut deferrable = false;
                let mut found = None;
                let candidate = |k: &&Keyed| {
                    k.predicate.is_none()
                        && if with_period {
                            k.exclusion.is_some()
                        } else {
                            k.unique || (k.exclusion.is_some() && k.constraint.is_some())
                        }
                };
                for k in parent.keys().filter(candidate) {
                    let mut on: Vec<usize> = Vec::new();
                    for l in &k.levels {
                        match l {
                            Level::Column(c) => on.push(*c),
                            Level::MonthUtc(_) | Level::Scaled(..) => on.clear(),
                        }
                    }
                    let mut a = on.clone();
                    let mut b = wanted.clone();
                    a.sort_unstable();
                    b.sort_unstable();
                    if on.len() != k.levels.len() || a != b {
                        continue;
                    }
                    if with_period && on.last() != wanted.last() {
                        continue;
                    }
                    if k.constraint.as_ref().is_some_and(|c| c.deferrable) {
                        deferrable = true;
                        continue;
                    }
                    found = Some(k);
                    break;
                }
                match found {
                    Some(k) => (k, wanted),
                    None if deferrable => {
                        return Err(Error::new(format!(
                            "cannot use a deferrable unique constraint for referenced table \
                             \"{pname}\""
                        )))
                    }
                    None => {
                        return Err(Error::new(format!(
                            "there is no unique constraint matching given keys for referenced \
                             table \"{pname}\""
                        )))
                    }
                }
            }
        };
        if key.exclusion.is_some() && !with_period {
            return Err(Error::new(
                "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS",
            ));
        }
        if r.period {
            for (when, (code, _)) in [("ON UPDATE", &r.on_update), ("ON DELETE", &r.on_delete)] {
                if *code != 'a' {
                    return Err(Error::new(format!(
                        "unsupported {when} action for foreign key constraint using PERIOD"
                    )));
                }
            }
        }
        if columns.len() != parent_columns.len() {
            return Err(Error::new(
                "number of referencing and referenced columns for foreign key disagree",
            ));
        }
        // A PERIOD is compared by its key's `&&`, and is a range or a multirange.
        if with_period {
            let last = key.opclasses.len() - 1;
            if key.method != Method::Gist
                || crate::methods::exclusion_operator(
                    key.method,
                    &key.opclasses[last],
                    parent.columns[key.levels[last].column()].ty,
                    "&&",
                )
                .is_err()
            {
                return Err(Error::new(
                    "could not identify an overlaps operator for foreign key",
                ));
            }
        }
        let name = match &r.name {
            Some(n) => {
                let own = t
                    .keys()
                    .filter(|k| k.constraint.is_some())
                    .map(|k| k.name.as_str())
                    .chain(t.checks.iter().map(|k| k.name.as_str()))
                    .chain(t.foreign_keys.iter().map(|k| k.name.as_str()));
                if own.into_iter().any(|x| x == n) {
                    return Err(Error::new(format!(
                        "constraint \"{n}\" for relation \"{}\" already exists",
                        bare(child)
                    )));
                }
                n.clone()
            }
            None => choose(
                bare(child),
                Some(&name_addition(&r.columns)),
                "fkey",
                &|n| self.constraint(schema, n) || t.foreign_keys.iter().any(|k| k.name == n),
            ),
        };
        for (&c, &p) in columns.iter().zip(&parent_columns) {
            let (a, b) = (t.columns[c].ty, parent.columns[p].ty);
            if !comparable(a, b) {
                return Err(Error::new(format!(
                    "foreign key constraint \"{name}\" cannot be implemented: key columns \"{}\" \
                     of the referencing table and \"{}\" of the referenced table are of types {} \
                     and {}, and the snail compares a reference with a key of its own type only, \
                     an integer with any integer",
                    t.columns[c].name,
                    parent.columns[p].name,
                    a.pg_name(),
                    b.pg_name()
                )));
            }
        }
        if with_period {
            let p = parent_columns[parent_columns.len() - 1];
            if !crate::spans::is_period(parent.columns[p].ty) {
                return Err(Error::new(
                    "invalid type for PERIOD part of foreign key: only range and multirange are \
                     supported",
                ));
            }
        }
        let action = |(code, listed): (char, Vec<String>)| -> Result<Action> {
            let mut set: Vec<usize> = Vec::new();
            for n in &listed {
                match column_of(t, n) {
                    Ok(c) if columns.contains(&c) => {
                        if !set.contains(&c) {
                            set.push(c);
                        }
                    }
                    _ => {
                        return Err(Error::new(format!(
                            "column \"{n}\" referenced in ON DELETE SET action must be part of \
                             foreign key"
                        )))
                    }
                }
            }
            Ok(match code {
                'r' => Action::Restrict,
                'c' => Action::Cascade,
                'n' => Action::SetNull(set),
                'd' => Action::SetDefault(set),
                _ => Action::NoAction,
            })
        };
        let on_delete = action(r.on_delete)?;
        let on_update = action(r.on_update)?;
        for &c in &columns {
            match &t.columns[c].generated {
                Some(g) if !g.stored => {
                    return Err(Error::new(
                        "foreign key constraints on virtual generated columns are not supported",
                    ))
                }
                Some(_) => {
                    for (when, a) in [("ON UPDATE", &on_update), ("ON DELETE", &on_delete)] {
                        let refused = match a {
                            Action::SetNull(_) | Action::SetDefault(_) => true,
                            Action::Cascade => when == "ON UPDATE",
                            _ => false,
                        };
                        if refused {
                            return Err(Error::new(format!(
                                "invalid {when} action for foreign key constraint containing \
                                 generated column"
                            )));
                        }
                    }
                }
                None => {}
            }
        }
        Ok(ForeignKey {
            name,
            columns,
            parent: parent.name.clone(),
            parent_columns,
            key: key.name.clone(),
            full: r.full,
            on_delete,
            on_update,
            deferrable: r.deferrable,
            deferred: r.deferred,
            enforced: r.enforced,
            valid: r.enforced && (made || !r.not_valid),
            period: with_period,
        })
    }

    fn create_index(&mut self, tokens: &[Token]) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        let (kept, params) = index_extras(tokens)?;
        let st = Parser::new(&crate::grammar::POSTGRES)
            .with_tokens(kept.clone())
            .parse_statement()?;
        let Statement::CreateIndex(i) = &st else {
            return Err(Error::new(
                "CREATE INDEX: sqlparser reads another statement",
            ));
        };
        let table_name = object_name(&i.table_name);
        let shown = i
            .name
            .as_ref()
            .map(object_name)
            .unwrap_or_else(|| format!("an index on {table_name}"));
        read_as_written(&kept, &st, &format!("CREATE INDEX {shown}"))?;
        let schema = schema_of(&table_name).to_string();
        let functions = self.functions.clone();
        let t = self
            .table(&table_name)
            .ok_or_else(|| Error::new(format!("relation \"{table_name}\" does not exist")))?;
        if i.r#async || !i.index_options.is_empty() || !i.alter_options.is_empty() {
            return Err(Error::new(format!("{shown}: {st} is not read")));
        }
        let method = match &i.using {
            None => Method::Btree,
            Some(m) => {
                let m = m.to_string().to_lowercase();
                Method::of_name(&m)
                    .ok_or_else(|| Error::new(format!("access method \"{m}\" does not exist")))?
            }
        };
        let am = method.name();
        if i.unique && !method.unique() {
            return Err(Error::new(format!(
                "access method \"{am}\" does not support unique indexes"
            )));
        }
        if !i.include.is_empty() && !method.includes() {
            return Err(Error::new(format!(
                "access method \"{am}\" does not support included columns"
            )));
        }
        if i.columns.len() > 1 && !method.several_keys() {
            return Err(Error::new(format!(
                "access method \"{am}\" does not support multicolumn indexes"
            )));
        }
        let (predicate, predicate_columns) = match &i.predicate {
            None => (None, Vec::new()),
            Some(p) => {
                // The predicate is read as a question's WHERE on the table alone.
                let alone = Catalog {
                    ddl: String::new(),
                    tables: vec![Table {
                        name: t.name.clone(),
                        columns: t.columns.clone(),
                        orders: Vec::new(),
                        checks: Vec::new(),
                        release: crate::regs::LATEST,
                        foreign_keys: Vec::new(),
                        monetary: crate::monetary::Monetary::C,
                        brins: Vec::new(),
                        text_search: crate::tsearch::TextSearch::Simple,
                        format: crate::file::FORMAT,
                        element_keys: Vec::new(),
                        elements_of: None,
                    }],
                    functions: self.functions.clone(),
                    types: self.types.clone(),
                    domains: self.domains.clone(),
                    sequences: self.sequences.clone(),
                    collations: self.collations.clone(),
                    views: Vec::new(),
                    release: crate::regs::LATEST,
                    monetary: crate::monetary::Monetary::C,
                    text_search: crate::tsearch::TextSearch::Simple,
                    collation: String::new(),
                    oids: Default::default(),
                };
                let text = p.to_string();
                // A fold's copy of the statement is read by the literal each mark stands for.
                let read_as = crate::deparse::analyze::unmarked(&text);
                let read = crate::read::partial::Predicate::read(&alone, &alone.tables[0], &read_as)
                    .map_err(
                    |e| {
                        Error::new(format!(
                            "{shown}: its predicate, WHERE {text}, is read only as comparisons of \
                             its columns with values, IS NULL and IS NOT NULL, and the truth tests: \
                             {e}"
                        ))
                    })?;
                (Some(text), read.columns())
            }
        };
        let mut levels = Vec::new();
        let mut desc = Vec::new();
        let mut nulls_first = Vec::new();
        let mut opclasses = Vec::new();
        let mut written = Vec::new();
        let mut keys: Vec<Expr> = Vec::new();
        for (n, c) in i.columns.iter().enumerate() {
            let s = shape(
                &c.column.expr,
                &table_name,
                None,
                &t.columns,
                &functions,
                &self.collations,
                &[],
            )
            .map_err(|e| Error::new(format!("{shown}: {e}")))?;
            if let Shape::Column(k) = s {
                if t.columns[k].is_virtual() {
                    return Err(Error::new(format!(
                        "{shown}: indexes on virtual generated columns are not supported"
                    )));
                }
            }
            let d = match &c.column.options.sort {
                None | Some(OrderBySort::Asc) => false,
                Some(OrderBySort::Desc) => true,
                Some(OrderBySort::Using(other)) => {
                    return Err(Error::new(format!(
                        "{shown}: {} USING {other} is not read",
                        c.column.expr
                    )))
                }
            };
            let default = match s {
                Shape::Column(k) => t.columns[k].ty.default_opclass(),
                Shape::Clock(_) => "timestamp_ops",
                Shape::MonthUtc(_) => "int2_ops",
                Shape::Scaled(..) => "float8_ops",
            };
            let key_type = match s {
                Shape::Column(k) => t.columns[k].ty,
                Shape::Clock(_) => ColumnType::Timestamp(None),
                Shape::MonthUtc(_) => ColumnType::Int2,
                Shape::Scaled(..) => ColumnType::Float8,
            };
            let named = c.operator_class.as_ref().map(|o| {
                let o = object_name(o);
                o.strip_prefix("pg_catalog.").unwrap_or(&o).to_string()
            });
            let op = match &c.operator_class {
                _ if method != Method::Btree => {
                    crate::methods::opclass(method, key_type, named.as_deref(), self.release)?
                }
                None => default.to_string(),
                Some(o) => {
                    let o = object_name(o);
                    let o = o.strip_prefix("pg_catalog.").unwrap_or(&o).to_string();
                    let text = matches!(s, Shape::Column(k) if crate::methods::read_as_text(t.columns[k].ty));
                    let bpchar = matches!(s, Shape::Column(k) if matches!(t.columns[k].ty, ColumnType::Bpchar(_)));
                    if o == "record_image_ops" {
                        return Err(Error::new(format!(
                            "{shown}: the operator class record_image_ops is not read: it orders \
                             records by PostgreSQL's in-memory layout of each type, which the \
                             snail does not keep"
                        )));
                    }
                    if o != default
                        && !(text && TEXT_OPCLASSES.contains(&o.as_str()))
                        && !(bpchar && o == "bpchar_pattern_ops")
                        && !(o == "array_ops" && crate::composed::is_vector(key_type))
                        && !(o == "cidr_ops" && crate::methods::is_inet(key_type))
                    {
                        return Err(Error::new(format!(
                            "{shown}: the operator class {o} is not read on {}",
                            c.column.expr
                        )));
                    }
                    o
                }
            };
            if let Shape::Column(k) = s {
                if t.columns[k].collated() && op.ends_with("_pattern_ops") {
                    return Err(Error::new(format!(
                        "{shown}: the operator class {op} orders {} by its bytes, which a column \
                         under a library collation is not cut on",
                        c.column.expr
                    )));
                }
            }
            if !method.orders() {
                if c.column.options.sort.is_some() {
                    return Err(Error::new(format!(
                        "access method \"{am}\" does not support ASC/DESC options"
                    )));
                }
                if c.column.options.nulls_first.is_some() {
                    return Err(Error::new(format!(
                        "access method \"{am}\" does not support NULLS FIRST/LAST options"
                    )));
                }
            }
            for (_, p) in params.iter().filter(|(k, _)| *k == n) {
                crate::methods::opclass_params(method, &op, p)?;
            }
            written.push(params.iter().find(|(k, _)| *k == n).map(|(_, p)| p.clone()));
            if d {
                notices.push(format!(
                    "{shown}: {} DESC is held rising, its NULLs where PostgreSQL's index read \
                     backwards holds them",
                    c.column.expr
                ));
            }
            nulls_first.push(c.column.options.nulls_first.unwrap_or(d) != d);
            desc.push(d);
            opclasses.push(op);
            levels.push(level(s));
            keys.push(c.column.expr.clone());
        }
        let mut include = Vec::new();
        for c in &i.include {
            let c = ident(c);
            let k =
                t.columns.iter().position(|x| x.name == c).ok_or_else(|| {
                    Error::new(format!("column \"{c}\" does not exist"))
                })?;
            if t.columns[k].is_virtual() {
                return Err(Error::new(format!(
                    "{shown}: indexes on virtual generated columns are not supported"
                )));
            }
            include.push(k);
        }
        let mut options = Vec::new();
        for w in &i.with {
            let (k, v) = match w {
                Expr::BinaryOp {
                    left,
                    op: sqlparser::ast::BinaryOperator::Eq,
                    right,
                } => (left.to_string().to_lowercase(), right.to_string()),
                other => return Err(Error::new(format!("{shown}: WITH ({other}) is not read"))),
            };
            let v = crate::methods::storage_param(method, &k, &v)
                .map_err(|e| Error::new(format!("{shown}: {e}")))?;
            if options.iter().any(|(x, _)| *x == k) {
                return Err(Error::new(format!(
                    "{shown}: parameter \"{k}\" specified more than once"
                )));
            }
            options.push((k, v));
        }
        if !options.is_empty() {
            notices.push(format!(
                "{shown}: WITH ({}) is read, and does nothing: a snail's blocks are cut by their \
                 counts",
                options
                    .iter()
                    .map(|(k, v)| format!("{k} = {v}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if i.concurrently {
            notices.push(format!(
                "{shown}: CONCURRENTLY is read, and does nothing: a snail lays an index out in one \
                 round"
            ));
        }
        let name = match &i.name {
            Some(n) => {
                n.0.last()
                    .map(|p| match p {
                        ObjectNamePart::Identifier(id) => ident(id),
                        other => other.to_string(),
                    })
                    .unwrap_or_default()
            }
            None => {
                let mut all = keys.clone();
                all.extend(i.include.iter().map(|c| Expr::Identifier(c.clone())));
                let addition = name_addition(&key_names(&all));
                choose(bare(&table_name), Some(&addition), "idx", &|n| {
                    self.relation(&schema, n)
                })
            }
        };
        if self.relation(&schema, &name) {
            if i.if_not_exists {
                notices.push(format!("relation \"{name}\" already exists, skipping"));
                return Ok(notices);
            }
            return Err(Error::new(format!("relation \"{name}\" already exists")));
        }
        if let Some(o) = opclasses.iter().find(|o| !crate::methods::held(method, o)) {
            return Err(Error::new(format!(
                "{shown}: USING {am} with the operator class {o} is not held yet"
            )));
        }
        for (o, l) in opclasses.iter().zip(&levels) {
            if let Some(e) = crate::methods::elements_refused(method, o, t.columns[l.column()].ty) {
                return Err(e);
            }
        }
        let elements = opclasses
            .iter()
            .filter(|o| crate::methods::holds_elements(method, o))
            .count();
        if elements > 0 {
            if !include.is_empty() {
                notices.push(format!(
                    "{shown}: INCLUDE is read, and does nothing: an element strand ends in the \
                     table's key, and its rows are found by it"
                ));
            }
            self.strands_at
                .push((self.tables.len(), table_name.clone(), name.clone()));
        }
        let keyed = Keyed {
            name,
            method,
            predicate,
            predicate_columns,
            exclusion: None,
            levels,
            include,
            unique: i.unique,
            nulls_first,
            nulls_distinct: i.nulls_distinct != Some(false),
            desc,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            opclasses,
            options,
            params: written,
            constraint: None,
        };
        count_in(
            &mut self.relations,
            in_schema(schema_of(&table_name), &keyed.name),
        );
        self.table_mut(&table_name).unwrap().indexes.push(keyed);
        Ok(notices)
    }

    /// The tables, each given its order and its indexes.
    fn finish(&self) -> Vec<std::result::Result<Table, (String, Error)>> {
        let tables: Vec<_> = self
            .tables
            .iter()
            .map(|d| finish_table(d).map_err(|e| (d.name.clone(), e)))
            .collect();
        let mut strands: Vec<(usize, Table)> = Vec::new();
        for (at, on, index) in &self.strands_at {
            let finished = tables.iter().filter_map(|t| t.as_ref().ok());
            let Some(t) = finished.into_iter().find(|t| t.name == *on) else {
                continue;
            };
            for k in t.element_keys.iter().filter(|k| k.index == *index) {
                strands.push((*at, crate::read::elements::strand_table(t, k)));
            }
        }
        let mut strands = strands.into_iter().peekable();
        let mut out = Vec::new();
        for (i, t) in tables.into_iter().enumerate() {
            while let Some((_, s)) = strands.next_if(|(at, _)| *at <= i) {
                out.push(Ok(s));
            }
            out.push(t);
        }
        out.extend(strands.map(|(_, s)| Ok(s)));
        out
    }
}

/// The order a table is given where its DDL names none, neither a primary key nor CLUSTER ON: every
/// column it stores whose type has a B-tree, in the columns' order, ascending, NULLs last, each
/// under its type's default operator class, as an index of them all names them; not unique, so
/// equal rows share a place. A column whose text a collation library orders is no level of it, so
/// that the order is the same in every build. With no such column the table lies as its rows
/// arrive: along a number of its own past its columns, its one level.
fn given(columns: &mut Vec<Column>) -> Keyed {
    let mut levels: Vec<usize> = (0..columns.len())
        .filter(|&c| {
            let col = &columns[c];
            !col.is_virtual() && col.ty.ordered() && !col.keyed_under_a_library()
        })
        .collect();
    if levels.is_empty() {
        levels.push(columns.len());
        columns.push(Column::arrival());
    }
    let n = levels.len();
    Keyed {
        name: crate::catalog::GIVEN.to_string(),
        method: Method::Btree,
        predicate: None,
        predicate_columns: Vec::new(),
        exclusion: None,
        keys: levels.iter().map(|&c| columns[c].name.clone()).collect(),
        opclasses: levels
            .iter()
            .map(|&c| columns[c].ty.default_opclass().to_string())
            .collect(),
        levels: levels.into_iter().map(Level::Column).collect(),
        include: Vec::new(),
        unique: false,
        nulls_first: vec![false; n],
        nulls_distinct: true,
        desc: vec![false; n],
        params: vec![None; n],
        options: Vec::new(),
        constraint: None,
    }
}

fn finish_table(d: &Declared) -> Result<Table> {
    let name = &d.name;
    let mut columns = d.columns.clone();
    let given = (d.clustered.is_none() && d.primary.is_none()).then(|| given(&mut columns));
    let columns = &columns;
    let all: Vec<usize> = (0..columns.len())
        .filter(|&c| !columns[c].is_virtual())
        .collect();
    let mut strands: Vec<Keyed> = d.uniques.iter().chain(d.indexes.iter()).cloned().collect();
    let brins = strands
        .iter()
        .filter(|s| s.method == Method::Brin)
        .map(|s| Brin {
            name: s.name.clone(),
            levels: s.levels.clone(),
            keys: s.keys.clone(),
            opclasses: s.opclasses.clone(),
            params: s.params.clone(),
            options: s.options.clone(),
            predicate: s.predicate.clone(),
        })
        .collect();
    strands.retain(|s| s.method != Method::Brin);
    let ordered = match &d.clustered {
        Some(c) if d.primary.as_ref().is_some_and(|p| &p.name == c) => d.primary.clone().unwrap(),
        Some(c) => {
            let pos = strands.iter().position(|s| &s.name == c).ok_or_else(|| {
                Error::new(format!(
                    "{name} is clustered on {c}, which is no index of it"
                ))
            })?;
            // The order holds every column, so what its index includes adds nothing.
            let mut s = strands.remove(pos);
            s.unique = s.unique
                || d.primary
                    .as_ref()
                    .is_some_and(|p| p.unique && p.levels == s.levels);
            s
        }
        None => match (&d.primary, given) {
            (Some(p), _) => p.clone(),
            (None, given) => given.expect("the order a table with no key is given"),
        },
    };
    if d.clustered.is_some() {
        if let Some(p) = &d.primary {
            if p.levels != ordered.levels {
                strands.insert(0, p.clone());
            }
        }
    }
    let mut element_keys = Vec::new();
    strands.retain_mut(|s| {
        let holds: Vec<bool> = s
            .opclasses
            .iter()
            .map(|o| crate::methods::holds_elements(s.method, o))
            .collect();
        if !holds.contains(&true) {
            return true;
        }
        for (n, (l, o)) in s.levels.iter().zip(&s.opclasses).enumerate() {
            if holds[n] {
                element_keys.push(crate::catalog::ElementKey {
                    index: s.name.clone(),
                    method: s.method,
                    column: l.column(),
                    opclass: o.clone(),
                    params: s.params.get(n).cloned().flatten(),
                    include: s.include.clone(),
                    options: s.options.clone(),
                    predicate: s.predicate.clone(),
                    strand: crate::read::elements::strand_name(name, &s.name, n),
                    key: n,
                });
            }
        }
        // Keys of another kind beside them are a strand of their own.
        let others: Vec<usize> = (0..holds.len()).filter(|&n| !holds[n]).collect();
        fn kept<T: Clone>(v: &[T], at: &[usize]) -> Vec<T> {
            at.iter().map(|&n| v[n].clone()).collect()
        }
        s.levels = kept(&s.levels, &others);
        s.keys = kept(&s.keys, &others);
        s.opclasses = kept(&s.opclasses, &others);
        s.params = kept(&s.params, &others);
        s.nulls_first = kept(&s.nulls_first, &others);
        s.desc = kept(&s.desc, &others);
        !others.is_empty()
    });
    let key: Vec<usize> = dedup(ordered.levels.iter().map(|l| l.column()));
    let mut orders = vec![Order {
        name: ordered.name,
        method: ordered.method,
        levels: ordered.levels,
        columns: all,
        unique: ordered.unique,
        table_order: true,
        nulls_first: ordered.nulls_first,
        nulls_distinct: ordered.nulls_distinct,
        desc: ordered.desc,
        keys: ordered.keys,
        opclasses: ordered.opclasses,
        params: ordered.params,
        include: ordered.include,
        options: ordered.options,
        constraint: ordered.constraint,
        clustered: d.clustered.is_some(),
        predicate: None,
        exclusion: ordered.exclusion,
    }];
    for s in strands {
        let held = dedup(
            s.levels
                .iter()
                .map(|l| l.column())
                .chain(s.include.iter().copied())
                .chain(key.iter().copied())
                .chain(s.predicate_columns.iter().copied()),
        );
        orders.push(Order {
            name: s.name,
            method: s.method,
            levels: s.levels,
            columns: held,
            unique: s.unique,
            table_order: false,
            nulls_first: s.nulls_first,
            nulls_distinct: s.nulls_distinct,
            desc: s.desc,
            keys: s.keys,
            opclasses: s.opclasses,
            params: s.params,
            include: s.include.clone(),
            options: s.options,
            constraint: s.constraint,
            clustered: false,
            predicate: s.predicate,
            exclusion: s.exclusion,
        });
    }
    for o in &orders {
        // xid and cid have only a hash operator class, so PostgreSQL keys no B-tree on them.
        let unordered = |l: &&Level| o.method == Method::Btree && !columns[l.column()].ty.ordered();
        if let Some(l) = o.levels.iter().find(unordered) {
            return Err(Error::new(format!(
                "data type {} has no default operator class for access method \"btree\"",
                columns[l.column()].ty.pg_name()
            )));
        }
    }
    Ok(Table {
        name: name.clone(),
        columns: columns.clone(),
        orders,
        checks: d.checks.clone(),
        release: crate::regs::LATEST,
        foreign_keys: d.foreign_keys.clone(),
        monetary: crate::monetary::Monetary::C,
        text_search: crate::tsearch::TextSearch::Simple,
        format: crate::file::FORMAT,
        element_keys,
        elements_of: None,
        brins,
    })
}

/// Whether a reference of type `a` is compared with a key of type `b` as the snail compares a key:
/// of one type, an integer with any integer, text with text, a numeric with one of its scale.
fn comparable(a: ColumnType, b: ColumnType) -> bool {
    use ColumnType::*;
    match (a, b) {
        (Int2 | Int4 | Int8, Int2 | Int4 | Int8) | (Text(_), Text(_)) => true,
        (Numeric { scale: x, .. }, Numeric { scale: y, .. }) => x == y,
        _ => a == b,
    }
}

fn dedup(items: impl Iterator<Item = usize>) -> Vec<usize> {
    let mut out = Vec::new();
    for i in items {
        if !out.contains(&i) {
            out.push(i);
        }
    }
    out
}

/// The names of a key's columns, each written plain.
fn key_columns(columns: &[sqlparser::ast::IndexColumn], table: &str) -> Result<Vec<String>> {
    columns
        .iter()
        .map(|c| match &c.column.expr {
            Expr::Identifier(i) if c.operator_class.is_none() => Ok(ident(i)),
            other => Err(Error::new(format!("{table}: a key on {other} is not read"))),
        })
        .collect()
}

/// Whether a key is DEFERRABLE; refused where it is INITIALLY DEFERRED, which checks it at the
/// commit, or NOT ENFORCED, which PostgreSQL refuses on a key.
fn deferrable(c: Option<&ConstraintCharacteristics>, what: &str) -> Result<bool> {
    let Some(c) = c else { return Ok(false) };
    if c.initially == Some(DeferrableInitial::Deferred) {
        return Err(Error::new(format!(
            "{what}: a key INITIALLY DEFERRED is not held: the snail checks a key at each \
             statement's end"
        )));
    }
    if c.enforced.is_some() {
        return Err(Error::new(format!(
            "{what}: a key is always enforced, so ENFORCED and NOT ENFORCED are not read on one"
        )));
    }
    Ok(c.deferrable == Some(true))
}

fn truth(b: bool) -> String {
    if b { "true" } else { "false" }.to_string()
}

/// Every fact of a catalog: its tables' columns, keys, CHECKs and indexes, and its types, domains,
/// sequences and collations. An expression is given as written; the comparison reads both sides'
/// alike.
pub(crate) fn facts(c: &Catalog) -> Vec<Fact> {
    let mut out = Vec::new();
    let mut fact = |kind: &str, object: &str, fact: &str, value: String| {
        out.push(Fact {
            kind: kind.into(),
            object: object.into(),
            fact: fact.into(),
            value,
        })
    };
    for t in c.tables.iter().filter(|t| t.declared()) {
        fact("table", &t.name, "columns", t.own_columns().len().to_string());
        for (i, col) in t.own_columns().iter().enumerate() {
            let o = format!("{}.{}", t.name, col.name);
            fact("column", &o, "position", (i + 1).to_string());
            fact(
                "column",
                &o,
                "type",
                col.domain.clone().unwrap_or_else(|| col.ty.pg_name()),
            );
            fact("column", &o, "not_null", truth(col.not_null));
            if let Some(d) = &col.default {
                fact("column", &o, "default", d.to_string());
            }
            if let Some(g) = &col.generated {
                let s = if g.stored { "stored" } else { "virtual" };
                fact("column", &o, "generated", s.into());
                fact("column", &o, "generation", g.expr.to_string());
            }
            if let Some(id) = &col.identity {
                let s = if id.always { "always" } else { "by default" };
                fact("column", &o, "identity", s.into());
            }
            if let Some(own) = collation_of(col.ty) {
                // A column of a domain naming no COLLATE takes its domain's, or its base domain's.
                let mut by_domain = None;
                let mut at = col.domain.clone();
                for _ in 0..32 {
                    let Some(n) = at.take() else { break };
                    let Some(d) = c.domains.iter().find(|x| crate::oids::full(&x.name) == crate::oids::full(&n)) else {
                        break;
                    };
                    if d.collation.is_some() {
                        by_domain = d.collation.clone();
                        break;
                    }
                    at = Some(d.base.clone());
                }
                fact(
                    "column",
                    &o,
                    "collation",
                    col.collation.clone().or(by_domain).unwrap_or_else(|| own.into()),
                );
            }
        }
        let names = |levels: &[Level]| {
            levels
                .iter()
                .map(|l| t.columns[l.column()].name.clone())
                .collect::<Vec<_>>()
                .join(",")
        };
        // The order a table is given where its DDL names none is no index of PostgreSQL's.
        for o in t
            .orders
            .iter()
            .filter(|o| !o.given() && !t.element_keys.iter().any(|k| k.index == o.name))
        {
            let obj = format!("{}.{}", t.name, o.name);
            if let Some(k) = &o.constraint {
                let ty = if k.primary { "p" } else { "u" };
                fact("constraint", &obj, "type", ty.into());
                fact("constraint", &obj, "columns", names(&o.levels));
                fact("constraint", &obj, "deferrable", truth(k.deferrable));
                fact("constraint", &obj, "period", truth(o.without_overlaps()));
                if !k.primary {
                    fact(
                        "constraint",
                        &obj,
                        "nulls_distinct",
                        truth(o.nulls_distinct),
                    );
                }
            }
            fact("index", &obj, "method", o.method.name().into());
            // PostgreSQL marks a key WITHOUT OVERLAPS's index unique, its EXCLUDE refusing the
            // same key.
            fact(
                "index",
                &obj,
                "unique",
                truth(o.unique || o.without_overlaps()),
            );
            fact(
                "index",
                &obj,
                "primary",
                truth(o.constraint.as_ref().is_some_and(|k| k.primary)),
            );
            fact("index", &obj, "keys", o.keys.join(","));
            fact(
                "index",
                &obj,
                "desc",
                o.desc
                    .iter()
                    .map(|&d| truth(d))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            // PostgreSQL's NULLS FIRST is in the index's own direction; the order's, as its values
            // lie rising.
            fact(
                "index",
                &obj,
                "nulls_first",
                o.nulls_first
                    .iter()
                    .zip(&o.desc)
                    .map(|(&f, &d)| truth(f != d))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            fact("index", &obj, "opclasses", o.opclasses.join(","));
            let include: Vec<String> = o
                .include
                .iter()
                .map(|&i| t.columns[i].name.clone())
                .collect();
            fact("index", &obj, "include", include.join(","));
            fact(
                "index",
                &obj,
                "options",
                o.options
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            fact("index", &obj, "nulls_distinct", truth(o.nulls_distinct));
            fact("index", &obj, "clustered", truth(o.clustered));
        }
        let mut told: Vec<&str> = Vec::new();
        for k in &t.element_keys {
            if told.contains(&k.index.as_str()) {
                continue;
            }
            told.push(&k.index);
            let all: Vec<_> = t
                .element_keys
                .iter()
                .filter(|x| x.index == k.index)
                .collect();
            let obj = format!("{}.{}", t.name, k.index);
            let names = |c: &[usize]| {
                let n: Vec<&str> = c.iter().map(|&i| t.columns[i].name.as_str()).collect();
                n.join(",")
            };
            // Each key in its place: the elements', and those of another kind beside them.
            let other = t.orders.iter().find(|o| o.name == k.index);
            let mut others = other.iter().flat_map(|o| o.levels.iter().zip(&o.opclasses));
            let total = all.len() + other.map_or(0, |o| o.levels.len());
            let each = |v: &str| vec![v; total].join(",");
            let (mut keys, mut classes): (Vec<usize>, Vec<&str>) = (Vec::new(), Vec::new());
            for n in 0..total {
                match all.iter().find(|x| x.key == n) {
                    Some(x) => {
                        keys.push(x.column);
                        classes.push(&x.opclass);
                    }
                    None => {
                        let (l, o) = others.next().expect("a key of another kind in its place");
                        keys.push(l.column());
                        classes.push(o);
                    }
                }
            }
            let options: Vec<String> = k.options.iter().map(|(a, b)| format!("{a}={b}")).collect();
            fact("index", &obj, "method", k.method.name().into());
            fact("index", &obj, "unique", truth(false));
            fact("index", &obj, "primary", truth(false));
            fact("index", &obj, "keys", names(&keys));
            fact("index", &obj, "desc", each("false"));
            fact("index", &obj, "nulls_first", each("false"));
            fact("index", &obj, "opclasses", classes.join(","));
            fact("index", &obj, "include", names(&k.include));
            fact("index", &obj, "options", options.join(","));
            fact("index", &obj, "nulls_distinct", truth(true));
            fact("index", &obj, "clustered", truth(false));
        }
        for b in &t.brins {
            let obj = format!("{}.{}", t.name, b.name);
            let none = || vec![truth(false); b.levels.len()].join(",");
            fact("index", &obj, "method", Method::Brin.name().into());
            fact("index", &obj, "unique", truth(false));
            fact("index", &obj, "primary", truth(false));
            fact("index", &obj, "keys", b.keys.join(","));
            fact("index", &obj, "desc", none());
            fact("index", &obj, "nulls_first", none());
            fact("index", &obj, "opclasses", b.opclasses.join(","));
            fact("index", &obj, "include", String::new());
            fact(
                "index",
                &obj,
                "options",
                b.options
                    .iter()
                    .map(|(k, v)| format!("{k}={v}"))
                    .collect::<Vec<_>>()
                    .join(","),
            );
            fact("index", &obj, "nulls_distinct", truth(true));
            fact("index", &obj, "clustered", truth(false));
        }
        for k in &t.checks {
            let obj = format!("{}.{}", t.name, k.name);
            fact("constraint", &obj, "type", "c".into());
            fact("constraint", &obj, "check", k.expr.to_string());
            fact("constraint", &obj, "enforced", truth(k.enforced));
            fact("constraint", &obj, "validated", truth(k.valid));
        }
        let names_of = |table: &Table, columns: &[usize]| {
            columns
                .iter()
                .map(|&i| table.columns[i].name.clone())
                .collect::<Vec<_>>()
                .join(",")
        };
        for k in &t.foreign_keys {
            let obj = format!("{}.{}", t.name, k.name);
            let code = |a: &Action| match a {
                Action::NoAction => "a",
                Action::Restrict => "r",
                Action::Cascade => "c",
                Action::SetNull(_) => "n",
                Action::SetDefault(_) => "d",
            };
            fact("constraint", &obj, "type", "f".into());
            fact("constraint", &obj, "columns", names_of(t, &k.columns));
            if let Some(p) = c.table(&k.parent) {
                fact(
                    "constraint",
                    &obj,
                    "references",
                    format!("{}({})", p.name, names_of(p, &k.parent_columns)),
                );
            }
            fact("constraint", &obj, "key", k.key.clone());
            fact(
                "constraint",
                &obj,
                "match",
                if k.full { "f" } else { "s" }.into(),
            );
            fact("constraint", &obj, "on_delete", code(&k.on_delete).into());
            fact("constraint", &obj, "on_update", code(&k.on_update).into());
            let listed = match &k.on_delete {
                Action::SetNull(c) | Action::SetDefault(c) => names_of(t, c),
                _ => String::new(),
            };
            fact("constraint", &obj, "delete_columns", listed);
            fact("constraint", &obj, "deferrable", truth(k.deferrable));
            fact("constraint", &obj, "period", truth(k.period));
            fact("constraint", &obj, "deferred", truth(k.deferred));
            fact("constraint", &obj, "enforced", truth(k.enforced));
            fact("constraint", &obj, "validated", truth(k.valid));
        }
    }
    for ty in &c.types {
        match &ty.kind {
            TypeKind::Enum { labels } => {
                fact("type", &ty.name, "kind", "enum".into());
                fact(
                    "type",
                    &ty.name,
                    "labels",
                    labels
                        .iter()
                        .map(|l| format!("'{}'", l.replace('\'', "''")))
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            TypeKind::Composite { fields } => {
                fact("type", &ty.name, "kind", "composite".into());
                fact(
                    "type",
                    &ty.name,
                    "fields",
                    fields
                        .iter()
                        .map(|(n, t, k)| match k {
                            Some(k) => format!("{n} {t} COLLATE {k}"),
                            None => format!("{n} {t}"),
                        })
                        .collect::<Vec<_>>()
                        .join(","),
                );
            }
            TypeKind::Range(r) => {
                fact("type", &ty.name, "kind", "range".into());
                fact("type", &ty.name, "subtype", r.subtype.clone());
                fact("type", &ty.name, "multirange", r.multirange.clone());
                for (f, v) in [
                    ("opclass", &r.opclass),
                    ("collation", &r.collation),
                    ("canonical", &r.canonical),
                    ("subtype_diff", &r.subtype_diff),
                ] {
                    if let Some(v) = v {
                        fact("type", &ty.name, f, v.clone());
                    }
                }
                fact("type", &r.multirange, "kind", "multirange".into());
                fact("type", &r.multirange, "range", ty.name.clone());
            }
        }
    }
    for d in &c.domains {
        fact("domain", &d.name, "base", d.base.clone());
        fact("domain", &d.name, "not_null", truth(d.not_null));
        if let Some(e) = &d.default {
            fact("domain", &d.name, "default", e.to_string());
        }
        if let Some(k) = &d.collation {
            fact("domain", &d.name, "collation", k.clone());
        }
        for k in &d.checks {
            let obj = format!("{}.{}", d.name, k.name);
            fact("domain constraint", &obj, "check", k.check.to_string());
            fact("domain constraint", &obj, "validated", truth(k.valid));
        }
    }
    for s in &c.sequences {
        let n = &s.name;
        fact("sequence", n, "type", s.ty.column().pg_name());
        fact("sequence", n, "start", s.start.to_string());
        fact("sequence", n, "increment", s.increment.to_string());
        fact("sequence", n, "min", s.min.to_string());
        fact("sequence", n, "max", s.max.to_string());
        fact("sequence", n, "cache", s.cache.to_string());
        fact("sequence", n, "cycle", truth(s.cycle));
        fact("sequence", n, "last_value", s.last_value.to_string());
        fact("sequence", n, "is_called", truth(s.is_called));
        fact(
            "sequence",
            n,
            "owned_by",
            s.owned_by
                .as_ref()
                .map(|(t, c)| format!("{t}.{c}"))
                .unwrap_or_default(),
        );
        fact("sequence", n, "identity", truth(s.identity));
    }
    for k in &c.collations {
        fact("collation", &k.name, "provider", k.provider.clone());
        if k.provider == "libc" {
            let base = k.locale.clone().unwrap_or_default();
            fact(
                "collation",
                &k.name,
                "collate",
                k.lc_collate.clone().unwrap_or(base.clone()),
            );
            fact(
                "collation",
                &k.name,
                "ctype",
                k.lc_ctype.clone().unwrap_or(base),
            );
        } else {
            fact(
                "collation",
                &k.name,
                "locale",
                k.locale.clone().unwrap_or_default(),
            );
        }
        fact(
            "collation",
            &k.name,
            "deterministic",
            truth(k.deterministic),
        );
        if let Some(r) = &k.rules {
            fact("collation", &k.name, "rules", r.clone());
        }
    }
    // Each view's columns, by the names its column list gives and its question returns.
    for v in &c.views {
        let bound = crate::grammar::bound(&v.query);
        let parsed = crate::read::image::parse_sql(bound.as_deref().unwrap_or(&v.query));
        let made = match parsed.as_deref() {
            Ok([Statement::Query(q)]) => crate::read::view_columns(c, q).unwrap_or_default(),
            _ => Vec::new(),
        };
        let named: Vec<String> = made
            .iter()
            .enumerate()
            .map(|(i, n)| v.columns.get(i).unwrap_or(n).clone())
            .collect();
        fact("view", &v.name, "columns", named.join(","));
    }
    out.sort();
    out
}
/// Calls `f` on an expression and on each expression inside it that the snail reads.
pub(crate) fn each_expr(e: &Expr, f: &mut dyn FnMut(&Expr)) {
    f(e);
    match e {
        Expr::BinaryOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => {
            each_expr(left, f);
            each_expr(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::Extract { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr)
        | Expr::IsNormalized { expr, .. } => each_expr(expr, f),
        Expr::Between {
            expr, low, high, ..
        } => {
            each_expr(expr, f);
            each_expr(low, f);
            each_expr(high, f);
        }
        Expr::InList { expr, list, .. } => {
            each_expr(expr, f);
            for x in list {
                each_expr(x, f);
            }
        }
        Expr::Like { expr, pattern, .. } | Expr::ILike { expr, pattern, .. } => {
            each_expr(expr, f);
            each_expr(pattern, f);
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            each_expr(timestamp, f);
            each_expr(time_zone, f);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(x) = operand {
                each_expr(x, f);
            }
            for c in conditions {
                each_expr(&c.condition, f);
                each_expr(&c.result, f);
            }
            if let Some(x) = else_result {
                each_expr(x, f);
            }
        }
        Expr::Function(func) => {
            if let FunctionArguments::List(l) = &func.args {
                for a in &l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(x),
                        ..
                    } = a
                    {
                        each_expr(x, f);
                    }
                }
            }
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            each_expr(left, f);
            each_expr(right, f);
        }
        Expr::Array(a) => {
            for x in &a.elem {
                each_expr(x, f);
            }
        }
        Expr::Tuple(xs) => {
            for x in xs {
                each_expr(x, f);
            }
        }
        Expr::SimilarTo { expr, pattern, .. } => {
            each_expr(expr, f);
            each_expr(pattern, f);
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            each_expr(expr, f);
            for x in [substring_from, substring_for].into_iter().flatten() {
                each_expr(x, f);
            }
        }
        Expr::Trim {
            expr, trim_what, ..
        } => {
            each_expr(expr, f);
            if let Some(x) = trim_what {
                each_expr(x, f);
            }
        }
        Expr::Position { expr, r#in } => {
            each_expr(expr, f);
            each_expr(r#in, f);
        }
        Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => each_expr(expr, f),
        Expr::Overlay {
            expr,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            each_expr(expr, f);
            each_expr(overlay_what, f);
            each_expr(overlay_from, f);
            if let Some(x) = overlay_for {
                each_expr(x, f);
            }
        }
        _ => {}
    }
}

/// What a foreign key's action writes where a parent's row is deleted or its key updated: nothing,
/// the rows that reference it, or the columns it sets in them.
enum Writes {
    Nothing,
    Rows,
    Columns(Vec<usize>),
}

fn writes(k: &ForeignKey, action: &Action, deleting: bool) -> Writes {
    match action {
        Action::NoAction | Action::Restrict => Writes::Nothing,
        Action::Cascade if deleting => Writes::Rows,
        _ => Writes::Columns(k.set_by(action)),
    }
}

/// Whether two keys on one parent, acting on one of its rows deleted or updated, meet: one writes
/// rows or columns the other reads or writes, and the two are not one key written twice.
fn meet(k: &ForeignKey, a: &Action, o: &ForeignKey, b: &Action, deleting: bool) -> bool {
    let pairs = |f: &ForeignKey| {
        let mut p: Vec<(usize, usize)> = f
            .columns
            .iter()
            .copied()
            .zip(f.parent_columns.iter().copied())
            .collect();
        p.sort();
        p
    };
    if a == b && pairs(k) == pairs(o) {
        return false;
    }
    let reads = |set: &[usize], f: &ForeignKey| set.iter().any(|c| f.columns.contains(c));
    match (writes(k, a, deleting), writes(o, b, deleting)) {
        (Writes::Nothing, Writes::Nothing) | (Writes::Rows, Writes::Rows) => false,
        (Writes::Rows, Writes::Nothing) | (Writes::Nothing, Writes::Rows) => true,
        (Writes::Rows, Writes::Columns(c)) | (Writes::Nothing, Writes::Columns(c)) => reads(&c, k),
        (Writes::Columns(c), Writes::Rows) | (Writes::Columns(c), Writes::Nothing) => reads(&c, o),
        (Writes::Columns(c), Writes::Columns(d)) => reads(&c, o) || reads(&d, k),
    }
}

/// Refuses a foreign key that meets another enforced key of its table on the same parent, ON
/// DELETE or ON UPDATE: PostgreSQL acts on the two in the order its names for their triggers sort.
fn parting(child: &str, keys: &[ForeignKey], k: &ForeignKey) -> Result<()> {
    if !k.enforced {
        return Ok(());
    }
    for o in keys.iter().filter(|o| o.enforced && o.parent == k.parent) {
        for (a, b, deleting) in [
            (&k.on_delete, &o.on_delete, true),
            (&k.on_update, &o.on_update, false),
        ] {
            if meet(k, a, o, b, deleting) {
                return Err(Error::new(format!(
                    "foreign keys \"{}\" and \"{}\" of relation \"{}\" both act ON {} of \"{}\", \
                     one writing what the other reads, and PostgreSQL's answer follows the order \
                     their triggers fire in: such a pair is not read",
                    o.name,
                    k.name,
                    bare(child),
                    if deleting { "DELETE" } else { "UPDATE" },
                    bare(&k.parent)
                )));
            }
        }
    }
    Ok(())
}

/// The columns an expression reads, each once, in the order it first reads them.
pub(crate) fn references(e: &Expr) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    each_expr(e, &mut |x| {
        let n = match x {
            Expr::Identifier(i) => Some(ident(i)),
            Expr::CompoundIdentifier(p) => p.last().map(ident),
            _ => None,
        };
        if let Some(n) = n {
            if !out.contains(&n) {
                out.push(n);
            }
        }
    });
    out
}

/// `nextval('<sequence>'::regclass)`, as PostgreSQL writes a serial's DEFAULT.
fn nextval_expr(sequence: &str) -> Expr {
    expression(
        &Tokenizer::new(
            &PostgreSqlDialect {},
            &format!("nextval('{}'::regclass)", sequence.replace('\'', "''")),
        )
        .tokenize()
        .unwrap(),
    )
    .unwrap()
}

/// A routine's body written in SQL's own words, taken out of its statement's words before
/// sqlparser reads the rest: the expression after RETURN, or the statements of BEGIN ATOMIC.
pub(crate) enum SqlBody {
    Return(String),
    Atomic(Vec<String>),
}

/// A CREATE FUNCTION's words without its body in SQL's own words, and that body: RETURN to the
/// statement's end, or BEGIN ATOMIC to its END, each found outside parentheses.
pub(crate) fn sql_body(tokens: &[Token]) -> Result<(Vec<Token>, Option<SqlBody>)> {
    let word = |t: Option<&Token>| match t {
        Some(Token::Word(w)) if w.quote_style.is_none() => w.value.to_ascii_uppercase(),
        _ => String::new(),
    };
    let solid = |from: usize| {
        tokens
            .get(from..)
            .unwrap_or(&[])
            .iter()
            .position(|t| !matches!(t, Token::Whitespace(_)))
            .map(|i| from + i)
    };
    let mut parens = 0i64;
    for (i, t) in tokens.iter().enumerate() {
        match t {
            Token::LParen => parens += 1,
            Token::RParen => parens -= 1,
            Token::Word(_) if parens == 0 => match word(Some(t)).as_str() {
                "RETURN" => {
                    let text = written(&tokens[i + 1..]);
                    return Ok((tokens[..i].to_vec(), Some(SqlBody::Return(text))));
                }
                "BEGIN" if solid(i + 1).is_some_and(|j| word(tokens.get(j)) == "ATOMIC") => {
                    let start = solid(i + 1).unwrap_or(i) + 1;
                    let (mut depth, mut inner) = (1usize, 0i64);
                    let mut statements: Vec<Vec<Token>> = vec![Vec::new()];
                    let mut end = None;
                    for (j, u) in tokens.iter().enumerate().skip(start) {
                        match u {
                            Token::LParen => inner += 1,
                            Token::RParen => inner -= 1,
                            Token::SemiColon if depth == 1 => {
                                statements.push(Vec::new());
                                continue;
                            }
                            Token::Word(_) if inner == 0 => match word(Some(u)).as_str() {
                                "BEGIN" | "CASE" => depth += 1,
                                "END" => {
                                    depth -= 1;
                                    if depth == 0 {
                                        end = Some(j);
                                        break;
                                    }
                                }
                                _ => {}
                            },
                            _ => {}
                        }
                        statements.last_mut().expect("a statement").push(u.clone());
                    }
                    let Some(end) = end else {
                        return Err(Error::new("syntax error at end of input"));
                    };
                    if let Some(k) = solid(end + 1) {
                        return Err(Error::new(format!(
                            "syntax error at or near \"{}\"",
                            tokens[k]
                        )));
                    }
                    let statements = statements
                        .iter()
                        .map(|s| written(s))
                        .filter(|s| !s.is_empty())
                        .collect();
                    return Ok((tokens[..i].to_vec(), Some(SqlBody::Atomic(statements))));
                }
                _ => {}
            },
            _ => {}
        }
    }
    Ok((tokens.to_vec(), None))
}

/// An expression written to the end of its text, refused as PostgreSQL's grammar refuses a word
/// after it.
fn whole_expression(text: &str) -> Result<()> {
    let tokens = crate::read::image::tokens(text)?;
    let mut p = Parser::new(&crate::grammar::POSTGRES).with_tokens_with_locations(tokens);
    p.parse_expr()?;
    match p.next_token().token {
        Token::EOF => Ok(()),
        t => Err(Error::new(format!("syntax error at or near \"{t}\""))),
    }
}

/// A body's one statement read: a SELECT of one expression and nothing else, its expression; any
/// other question, as a subquery of it, which a call runs as its statement. A question of other
/// than one column is refused as PostgreSQL refuses it when the function is made.
fn body_expression(
    name: &str,
    text: &str,
    returns: ColumnType,
    relations: &crate::read::own::Relations,
) -> Result<Expr> {
    let refused = |e: Error| Error::new(format!("{name}: its body: {e}"));
    // Its words as a statement's are read, then its operators as PostgreSQL's grammar binds them.
    let text = crate::read::scanned_unseeded(text, crate::regs::LATEST).map_err(refused)?;
    let bound = crate::grammar::bound(&text);
    let parsed =
        crate::read::image::parse_sql(bound.as_deref().unwrap_or(&text)).map_err(refused)?;
    let [Statement::Query(q)] = parsed.as_slice() else {
        return Err(Error::new(format!("{name}: its body is not one SELECT")));
    };
    let alone = q.with.is_none()
        && q.order_by.is_none()
        && q.limit_clause.is_none()
        && q.fetch.is_none()
        && q.locks.is_empty();
    if let SetExpr::Select(s) = q.body.as_ref() {
        let grouped = !matches!(
            &s.group_by,
            sqlparser::ast::GroupByExpr::Expressions(g, m) if g.is_empty() && m.is_empty()
        );
        if alone
            && s.from.is_empty()
            && s.selection.is_none()
            && s.having.is_none()
            && s.distinct.is_none()
            && !grouped
            && s.projection.len() == 1
        {
            return match &s.projection[0] {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    Ok(e.clone())
                }
                _ => Err(Error::new(format!("{name}: its body is not one expression"))),
            };
        }
    }
    if crate::read::own::columns_written(q, relations).is_some_and(|n| n != 1) {
        return Err(one_column(returns));
    }
    Ok(Expr::Subquery(q.clone()))
}

/// PostgreSQL's refusal of a function whose last statement returns other than one column.
pub(crate) fn one_column(returns: ColumnType) -> Error {
    Error::new(format!(
        "return type mismatch in function declared to return {}\nDETAIL:  Final statement must \
         return exactly one column.",
        returns.pg_name()
    ))
}

/// A function written in SQL: its name, its parameters' types, names and DEFAULTs, and its body's
/// one expression, written after AS, after RETURN or inside BEGIN ATOMIC (`sql`, taken out of its
/// words). Refused in PostgreSQL's words where its CREATE FUNCTION refuses it.
fn function(
    f: &sqlparser::ast::CreateFunction,
    sql: Option<SqlBody>,
    held: &dyn Fn(&DataType) -> Result<ColumnType>,
    relations: &crate::read::own::Relations,
) -> Result<Function> {
    let name = object_name(&f.name);
    let language = f.language.as_ref().map(ident);
    let quoted = match &f.function_body {
        None => None,
        Some(
            CreateFunctionBody::AsBeforeOptions {
                body: Expr::Value(v),
                link_symbol: None,
            }
            | CreateFunctionBody::AsAfterOptions(Expr::Value(v)),
        ) => match &v.value {
            Value::DollarQuotedString(s) => Some(s.value.clone()),
            Value::SingleQuotedString(s) => Some(s.clone()),
            _ => return Err(Error::new(format!("{name}: its body is not read"))),
        },
        Some(_) => return Err(Error::new(format!("{name}: its body is not read"))),
    };
    match (&language, &sql) {
        (None, None) => return Err(Error::new("no language specified")),
        (Some(l), None) if l != "sql" => {
            return Err(Error::new(format!("{name}: only LANGUAGE sql is read")))
        }
        _ => {}
    }
    let (mut args, mut arg_names, mut defaults) = (Vec::new(), Vec::new(), Vec::new());
    for a in f.args.as_deref().unwrap_or(&[]) {
        let ty = crate::read::own::parameter_type(held(&a.data_type)?);
        match &a.mode {
            None | Some(sqlparser::ast::ArgMode::In) => {}
            Some(m) => {
                return Err(Error::new(format!(
                    "{name}: a parameter of mode {m} is not read"
                )))
            }
        }
        let named = a.name.as_ref().map(ident);
        if let Some(n) = &named {
            if arg_names.iter().flatten().any(|x: &String| x == n) {
                return Err(Error::new(format!(
                    "parameter name \"{n}\" used more than once"
                )));
            }
        }
        match &a.default_expr {
            Some(e) => defaults.push(e.clone()),
            None if !defaults.is_empty() => {
                return Err(Error::new(
                    "input parameters after one with a default value must also have defaults",
                ))
            }
            None => {}
        }
        args.push(ty);
        arg_names.push(named);
    }
    if f.temporary
        || f.or_alter
        || f.security.is_some()
        || !f.set_params.is_empty()
        || f.using.is_some()
        || f.determinism_specifier.is_some()
        || f.options.is_some()
        || f.remote_connection.is_some()
    {
        return Err(Error::new(format!(
            "{name}: only its arguments, RETURNS, LANGUAGE, its volatility, STRICT, PARALLEL and its body are read"
        )));
    }
    let returns = match &f.return_type {
        Some(sqlparser::ast::FunctionReturnType::DataType(dt)) => {
            held_type(dt).map_err(|e| Error::new(format!("{name}: RETURNS {e}")))?
        }
        Some(sqlparser::ast::FunctionReturnType::SetOf(_)) => {
            return Err(Error::new(format!("{name}: RETURNS SETOF is not read")))
        }
        None => return Err(Error::new(format!("{name}: function result type must be specified"))),
    };
    let volatility = match f.behavior {
        Some(sqlparser::ast::FunctionBehavior::Immutable) => 'i',
        Some(sqlparser::ast::FunctionBehavior::Stable) => 's',
        Some(sqlparser::ast::FunctionBehavior::Volatile) | None => 'v',
    };
    let strict = matches!(
        f.called_on_null,
        Some(
            sqlparser::ast::FunctionCalledOnNull::ReturnsNullOnNullInput
                | sqlparser::ast::FunctionCalledOnNull::Strict
        )
    );
    let parallel = match f.parallel {
        Some(sqlparser::ast::FunctionParallel::Safe) => 's',
        Some(sqlparser::ast::FunctionParallel::Restricted) => 'r',
        Some(sqlparser::ast::FunctionParallel::Unsafe) | None => 'u',
    };
    if sql.is_some() && quoted.is_some() {
        return Err(Error::new("duplicate function body specified"));
    }
    if sql.is_some() && language.as_deref().is_some_and(|l| l != "sql") {
        return Err(Error::new(
            "inline SQL function body only valid for language SQL",
        ));
    }
    // The statement as written, and as read: the string after AS; RETURN's expression, read as a
    // SELECT of it; or BEGIN ATOMIC's one statement, a RETURN or a SELECT.
    let (form, statement, selected) = match (sql, quoted) {
        // The string read as a statement, as CREATE FUNCTION checks it, its labels among it.
        (None, Some(text)) => {
            crate::read::ddl_labels(&text)?;
            (BodyForm::Quoted, text.clone(), text)
        }
        (Some(SqlBody::Return(text)), None) => {
            whole_expression(&text)?;
            let selected = format!("SELECT {text}");
            (BodyForm::Return, text, selected)
        }
        (Some(SqlBody::Atomic(statements)), None) => match statements.as_slice() {
            [] => {
                return Err(Error::new(format!(
                    "return type mismatch in function declared to return {}\nDETAIL:  Function's \
                     final statement must be SELECT or INSERT/UPDATE/DELETE/MERGE RETURNING.",
                    returns.pg_name()
                )))
            }
            [one] => match returned(one) {
                Some(text) => {
                    whole_expression(text)?;
                    (BodyForm::Atomic, one.clone(), format!("SELECT {text}"))
                }
                None => (BodyForm::Atomic, one.clone(), one.clone()),
            },
            _ => {
                return Err(Error::new(format!(
                    "{name}: a body of more than one statement is not read"
                )))
            }
        },
        _ => return Err(Error::new("no function body specified")),
    };
    let body = body_expression(&name, &selected, returns, relations)?;
    let source = match form {
        BodyForm::Quoted => statement.clone(),
        _ => String::new(),
    };
    Ok(Function {
        name,
        args,
        arg_names,
        defaults,
        body,
        form,
        statement,
        returns,
        volatility,
        strict,
        parallel,
        source,
    })
}

/// The expression of a RETURN statement, as written after its first word; none for another.
pub(crate) fn returned(statement: &str) -> Option<&str> {
    let t = statement.trim_start();
    let head = t.get(..6)?;
    let rest = &t[6..];
    (head.eq_ignore_ascii_case("return") && rest.starts_with(|c: char| c.is_whitespace() || c == '('))
        .then(|| rest.trim())
}

/// Reads a DDL a statement at a time. `strict` stops at the first statement refused, as a file's
/// own DDL is read.
pub(crate) fn read_ddl(ddl: &str, strict: bool) -> Result<Reading> {
    read_ddl_kept(ddl, strict, 0)
}

/// Reads a DDL as `read_ddl` does, its first `kept` statements as the file's own: a library
/// collation they name taken by a build without the feature `collations`.
pub(crate) fn read_ddl_kept(ddl: &str, strict: bool, kept: usize) -> Result<Reading> {
    read_ddl_at(ddl, strict, kept, crate::regs::LATEST)
}

/// Reads a DDL as `read_ddl_kept` does, each type's default operator classes `release`'s.
pub(crate) fn read_ddl_at(ddl: &str, strict: bool, kept: usize, release: u8) -> Result<Reading> {
    read_ddl_given(ddl, strict, kept, release, &[])
}

/// Reads a file's DDL as `read_ddl_at` does, strictly, each statement `record` names taking its
/// OIDs from the first it gives: refused where the record does not fit the text.
pub(crate) fn read_ddl_recorded(
    ddl: &str,
    kept: usize,
    record: &crate::oids::Record,
) -> Result<Reading> {
    read_ddl_given(ddl, true, kept, crate::regs::LATEST, record)
}

fn read_ddl_given(
    ddl: &str,
    strict: bool,
    kept: usize,
    release: u8,
    record: &crate::oids::Record,
) -> Result<Reading> {
    let mut draft = Draft {
        release,
        record: record.to_vec(),
        ..Draft::default()
    };
    let mut statements: Vec<StatementRead> = Vec::new();
    let all = statements_of(ddl)?;
    // The statements taken so far, by their place.
    let mut taken: Vec<usize> = Vec::new();
    for (at, tokens) in all.iter().enumerate() {
        draft.kept = at < kept;
        let (kind, object) = head(tokens);
        // Each statement is applied in place. Read strictly, the DDL stops at its first refusal;
        // otherwise a statement refused leaves the draft as the statements taken before it make it.
        let before = draft.lens();
        let applied = draft
            .apply(tokens)
            .and_then(|n| draft.no_schema_implied(before).map(|()| n));
        match &applied {
            Ok(_) => {
                draft.at = taken.len();
                draft.settle(before, &kind, &object, tokens);
                draft.keep_named_schemas();
                taken.push(at)
            }
            Err(_) if !strict => {
                let mut again = Draft {
                    release,
                    ..Draft::default()
                };
                for (place, &i) in taken.iter().enumerate() {
                    again.at = place;
                    again.kept = i < kept;
                    let (k, o) = head(&all[i]);
                    let before = again.lens();
                    again.apply(&all[i]).map_err(|e| {
                        Error::new(format!(
                            "{}: taken once and refused read again: {}",
                            written(&all[i]),
                            e.0
                        ))
                    })?;
                    again.settle(before, &k, &o, &all[i]);
                    again.keep_named_schemas();
                }
                draft = again;
            }
            Err(_) => draft.renamed.clear(),
        }
        let read = match applied {
            Ok(notices) => StatementRead {
                text: written(&tokens),
                kind,
                object,
                refused: None,
                notices,
            },
            Err(e) => {
                // A statement on a table refused before it is refused for that.
                let on = if kind.contains("INDEX") {
                    index_on(&written(&tokens))
                } else if kind == "ALTER TABLE" {
                    Some(object.clone())
                } else {
                    None
                };
                let why = match on {
                    Some(t)
                        if draft.table(&t).is_none()
                            && statements
                                .iter()
                                .any(|s| s.refused.is_some() && s.object == t) =>
                    {
                        format!("{object} is on {t}, which is refused")
                    }
                    _ => e.0,
                };
                StatementRead {
                    text: written(&tokens),
                    kind,
                    object,
                    refused: Some(why),
                    notices: Vec::new(),
                }
            }
        };
        let stop = strict && read.refused.is_some();
        statements.push(read);
        if stop {
            break;
        }
    }
    // A record that does not fit the text: the file's root and its catalog disagree.
    let whole = statements.iter().all(|s| s.refused.is_none());
    let past = draft
        .record
        .last()
        .filter(|r| whole && r.0 as usize >= taken.len())
        .map(|r| format!("it names a statement at {}, past the text's {}", r.0, taken.len()));
    if let Some(why) = draft.oids.misfit().map(str::to_string).or(past) {
        return Err(Error::new(format!(
            "the file's record of its OIDs does not fit its catalog: {why}"
        )));
    }
    // Each type the DDL declares is held whether a column names it or not, so a cast can name it.
    for t in &draft.types {
        let _ = draft.declared_type(t, &t.name);
    }
    let mut finished = draft.finish();
    // A table whose foreign key references a table refused is refused for it, in turn.
    loop {
        let refused: Vec<String> = finished
            .iter()
            .filter_map(|r| r.as_ref().err().map(|(n, _)| n.clone()))
            .collect();
        let mut more = false;
        for r in &mut finished {
            if let Ok(t) = r {
                if let Some(k) = t.foreign_keys.iter().find(|k| refused.contains(&k.parent)) {
                    let e = Error::new(format!(
                        "{}: its foreign key {} references {}, which is refused",
                        t.name, k.name, k.parent
                    ));
                    *r = Err((t.name.clone(), e));
                    more = true;
                }
            }
        }
        if !more {
            break;
        }
    }
    let mut tables = Vec::new();
    // A table refused once every statement is read was given OIDs as it was read: the OIDs are
    // then those the DDL kept hands out, read again whole.
    let refused_late = finished.iter().any(|r| r.is_err());
    for r in finished {
        match r {
            Ok(t) => tables.push(t),
            Err((name, e)) => {
                // The table and every statement that names it are refused.
                for s in &mut statements {
                    if s.refused.is_none()
                        && (s.object == name
                            || (s.kind.contains("INDEX")
                                && index_on(&s.text) == Some(name.clone())))
                    {
                        s.refused = Some(if s.object == name {
                            e.0.clone()
                        } else {
                            format!("{} is on {name}, which is refused", s.object)
                        });
                    }
                }
            }
        }
    }
    let names: Vec<String> = tables.iter().map(|t: &Table| t.name.clone()).collect();
    tables.retain(|t| t.elements_of.as_ref().is_none_or(|of| names.contains(of)));
    let taken: Vec<String> = statements
        .iter()
        .filter(|s| s.refused.is_none())
        .map(|s| format!("{};", s.text))
        .collect();
    let ddl = if taken.is_empty() {
        String::new()
    } else {
        format!("{}\n", taken.join("\n"))
    };
    let oids = if refused_late && !strict {
        read_ddl_at(&ddl, true, kept, release)?.catalog.oids
    } else {
        std::sync::Arc::new(draft.oids)
    };
    Ok(Reading {
        catalog: Catalog {
            ddl,
            tables,
            functions: draft.functions,
            types: draft.types,
            domains: draft.domains,
            sequences: draft.sequences,
            collations: draft.collations,
            views: draft.views,
            release: crate::regs::LATEST,
            monetary: crate::monetary::Monetary::C,
            text_search: crate::tsearch::TextSearch::Simple,
            collation: String::new(),
            oids,
        },
        statements,
    })
}

/// The number of statements a DDL holds.
pub(crate) fn statements_in(ddl: &str) -> Result<usize> {
    Ok(statements_of(ddl)?.len())
}

fn statements_of(ddl: &str) -> Result<Vec<Vec<Token>>> {
    statements(ddl)
}

/// SET CONSTRAINTS { ALL | name [, …] } { DEFERRED | IMMEDIATE }: the names, none for ALL, and
/// whether deferred; none where the statement is another.
pub(crate) fn set_constraints(tokens: &[Token]) -> Option<Result<(Option<Vec<String>>, bool)>> {
    let mut c = Cursor::new(tokens);
    if !c.kws(&["SET", "CONSTRAINTS"]) {
        return None;
    }
    let read = (|| {
        let names = if c.kw("ALL") {
            None
        } else {
            let mut out = vec![c.name()?];
            while c.token(Token::Comma) {
                out.push(c.name()?);
            }
            Some(out)
        };
        let deferred = if c.kw("DEFERRED") {
            true
        } else if c.kw("IMMEDIATE") {
            false
        } else {
            return Err(c.wanted("DEFERRED or IMMEDIATE"));
        };
        end(&c, "SET CONSTRAINTS")?;
        Ok((names, deferred))
    })();
    Some(read)
}

/// What a declaration locks, as it names it: a relation by its name, every relation of a schema it
/// drops, a schema itself, or a type by the words that name it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Locked {
    Relation(String),
    Schema(String),
    Namespace(String),
    Type(String),
}

/// The relations a declaration locks while it runs, in the modes PostgreSQL 18.6 takes: a DROP's,
/// ACCESS EXCLUSIVE, a DROP SCHEMA's on the schema too, a DROP TYPE's or DROP DOMAIN's on each type
/// it names; ALTER TABLE's, the strongest its actions
/// take (`AlterTableGetLockLevel`), and SHARE ROW EXCLUSIVE on a table a foreign key references;
/// CREATE INDEX's table, SHARE, or SHARE UPDATE EXCLUSIVE CONCURRENTLY; ALTER SEQUENCE's, SHARE ROW
/// EXCLUSIVE; COMMENT ON a relation or its column or constraint, SHARE UPDATE EXCLUSIVE; and a view
/// CREATE OR REPLACE VIEW replaces, ACCESS EXCLUSIVE. Any other declaration locks no relation.
pub(crate) fn locked(sql: &str) -> Vec<(Locked, crate::read::tablelock::Mode)> {
    use crate::read::tablelock::Mode;
    let Ok(tokens) = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize() else {
        return Vec::new();
    };
    let mut c = Cursor::new(&tokens);
    let mut out = Vec::new();
    // Every table a REFERENCES names from here on.
    let referenced = |c: &Cursor, out: &mut Vec<(Locked, Mode)>| {
        for k in c.i..c.t.len() {
            if Cursor::word_is(c.t.get(k), "REFERENCES") {
                let mut r = Cursor {
                    t: c.t[k + 1..].to_vec(),
                    i: 0,
                };
                if let Ok(name) = r.name() {
                    out.push((Locked::Relation(name), Mode::ShareRowExclusive));
                }
            }
        }
    };
    let names = |c: &mut Cursor| -> Vec<String> {
        let mut names = Vec::new();
        while let Ok(n) = c.name() {
            names.push(n);
            if !c.token(Token::Comma) {
                break;
            }
        }
        names
    };
    if c.kw("DROP") {
        for (word, domain) in [("TYPE", false), ("DOMAIN", true)] {
            if c.kw(word) {
                if let Ok(Form::DropTypes { names, .. }) = drop_types(&mut c, domain) {
                    for n in names {
                        out.push((Locked::Type(n.words), Mode::AccessExclusive));
                    }
                }
                return out;
            }
        }
        let schema = c.kw("SCHEMA");
        if !schema && !(c.kw("TABLE") || c.kw("VIEW") || c.kw("SEQUENCE") || c.kw("INDEX")) {
            return out;
        }
        c.kw("CONCURRENTLY");
        c.kws(&["IF", "EXISTS"]);
        for n in names(&mut c) {
            if schema {
                out.push((Locked::Namespace(n.clone()), Mode::AccessExclusive));
            }
            let l = if schema {
                Locked::Schema(n)
            } else {
                Locked::Relation(n)
            };
            out.push((l, Mode::AccessExclusive));
        }
    } else if c.kws(&["ALTER", "TABLE"]) {
        c.kws(&["IF", "EXISTS"]);
        c.kw("ONLY");
        let Ok(table) = c.name() else {
            return out;
        };
        c.token(Token::Mul);
        let mut mode = Mode::ShareUpdateExclusive;
        while !c.done() {
            let action = Cursor {
                t: c.item(),
                i: 0,
            };
            let first = |w: &[&str]| w.iter().any(|k| action.is(k));
            let words = |w: &[&str]| {
                (0..action.t.len()).any(|k| {
                    w.iter()
                        .enumerate()
                        .all(|(n, x)| Cursor::word_is(action.t.get(k + n), x))
                })
            };
            // SET ( … ) or RESET ( … ): a table's or a column's options.
            let options = (0..action.t.len()).any(|k| {
                (Cursor::word_is(action.t.get(k), "SET") || Cursor::word_is(action.t.get(k), "RESET"))
                    && action.t.get(k + 1) == Some(&Token::LParen)
            });
            let foreign = first(&["ADD"]) && words(&["FOREIGN", "KEY"]);
            let taken = if foreign {
                Mode::ShareRowExclusive
            } else if first(&["VALIDATE", "CLUSTER", "ATTACH"])
                || words(&["SET", "WITHOUT", "CLUSTER"])
                || words(&["SET", "STATISTICS"])
                || options
            {
                Mode::ShareUpdateExclusive
            } else if first(&["ENABLE", "DISABLE"]) && words(&["TRIGGER"]) {
                Mode::ShareRowExclusive
            } else {
                Mode::AccessExclusive
            };
            mode = mode.max(taken);
            referenced(&action, &mut out);
        }
        out.insert(0, (Locked::Relation(table), mode));
    } else if c.kws(&["ALTER", "SEQUENCE"]) {
        c.kws(&["IF", "EXISTS"]);
        if let Ok(s) = c.name() {
            out.push((Locked::Relation(s), Mode::ShareRowExclusive));
        }
    } else if c.kw("CREATE") {
        let replace = c.kws(&["OR", "REPLACE"]);
        c.kw("UNIQUE");
        if c.kw("INDEX") {
            let mode = match c.kw("CONCURRENTLY") {
                true => Mode::ShareUpdateExclusive,
                false => Mode::Share,
            };
            if let Some(t) = index_on(sql) {
                out.push((Locked::Relation(t), mode));
            }
        } else if replace && c.kw("VIEW") {
            if let Ok(v) = c.name() {
                out.push((Locked::Relation(v), Mode::AccessExclusive));
            }
        } else {
            while c.kw("UNLOGGED") || c.kw("TEMP") || c.kw("TEMPORARY") {}
            if c.kw("TABLE") {
                c.kws(&["IF", "NOT", "EXISTS"]);
                let made = c.name().unwrap_or_default();
                referenced(&c, &mut out);
                out.retain(|(l, _)| *l != Locked::Relation(made.clone()));
            }
        }
    } else if c.kws(&["COMMENT", "ON"]) {
        let relation = if c.kw("TABLE") || c.kw("VIEW") || c.kw("SEQUENCE") || c.kw("INDEX") {
            c.name().ok()
        } else if c.kw("COLUMN") {
            c.name()
                .ok()
                .and_then(|n| n.rsplit_once('.').map(|(r, _)| r.to_string()))
        } else if c.kw("CONSTRAINT") {
            let _ = c.ident();
            c.kw("ON").then(|| c.name().ok()).flatten()
        } else {
            None
        };
        if let Some(r) = relation {
            out.push((Locked::Relation(r), Mode::ShareUpdateExclusive));
        }
    }
    out
}

/// What a statement declares, where it declares something the catalog holds: CREATE TABLE, INDEX,
/// SEQUENCE, TYPE, DOMAIN, COLLATION, VIEW, FUNCTION or SCHEMA, ALTER TABLE, SEQUENCE, TYPE or
/// DOMAIN, a DROP of a schema, a relation, a type or a domain, or COMMENT ON.
pub(crate) fn declares(sql: &str) -> Option<&'static str> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize().ok()?;
    let (kind, _) = head(&tokens);
    Some(match kind.as_str() {
        "CREATE TABLE" => "CREATE TABLE",
        "CREATE INDEX" | "CREATE UNIQUE INDEX" => "CREATE INDEX",
        "CREATE SEQUENCE" => "CREATE SEQUENCE",
        "CREATE TYPE" => "CREATE TYPE",
        "CREATE DOMAIN" => "CREATE DOMAIN",
        "CREATE COLLATION" => "CREATE COLLATION",
        "ALTER TABLE" => "ALTER TABLE",
        "ALTER SEQUENCE" => "ALTER SEQUENCE",
        "ALTER TYPE" => "ALTER TYPE",
        "ALTER DOMAIN" => "ALTER DOMAIN",
        "CREATE VIEW" | "CREATE OR REPLACE VIEW" => "CREATE VIEW",
        "CREATE FUNCTION" => "CREATE FUNCTION",
        "CREATE SCHEMA" => "CREATE SCHEMA",
        "DROP SCHEMA" => "DROP SCHEMA",
        "DROP TABLE" => "DROP TABLE",
        "DROP VIEW" => "DROP VIEW",
        "DROP SEQUENCE" => "DROP SEQUENCE",
        "DROP INDEX" => "DROP INDEX",
        "DROP TYPE" => "DROP TYPE",
        "DROP DOMAIN" => "DROP DOMAIN",
        "DROP FUNCTION" => "DROP FUNCTION",
        "COMMENT ON" => "COMMENT",
        _ => return None,
    })
}

/// Whether a statement's notice is PostgreSQL's own, which a session hears as PostgreSQL's NOTICE:
/// what IF [NOT] EXISTS passed over, what a DROP took with it, an ICU locale's standard form. The
/// others are the snail's word on what it reads and does nothing with.
pub(crate) fn said_by_postgresql(notice: &str) -> bool {
    notice.ends_with(", skipping")
        || notice.starts_with("drop cascades to ")
        || notice.starts_with("using standard form ")
}

/// Where a DROP's refusal of a column it reaches turns from the relation it names to why.
const REACHED: &str = ", which it leaves: ";

/// The table a DROP's refusal names where the drop reaches a column of a table it leaves; none for
/// any other refusal.
pub(crate) fn reached_table(e: &Error) -> Option<String> {
    let first = e.0.lines().next()?;
    let rest = first.strip_prefix("the drop reaches column ")?;
    let (_, table) = rest.rsplit_once(" of table ")?;
    Some(table.split_once(REACHED)?.0.to_string())
}

/// A relation's name as text, as PostgreSQL's regclass reads it: its parts folded unless quoted.
pub(crate) fn regclass(text: &str) -> Result<String> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, text).tokenize()?;
    let mut c = Cursor::new(&tokens);
    let name = c.name()?;
    if !c.done() {
        return Err(Error::new(format!("'{text}' is no relation's name")));
    }
    Ok(name)
}
/// The table a CREATE INDEX is on.
fn index_on(text: &str) -> Option<String> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, text)
        .tokenize()
        .ok()?;
    let mut c = Cursor::new(&tokens);
    while !c.done() && !c.is("ON") {
        c.i += 1;
    }
    c.kw("ON");
    c.kw("ONLY");
    c.name().ok()
}

/// The first statement of a DDL that 0.1.x does not read, as its head names it: CREATE SCHEMA, and
/// DROP of a schema or a relation. None where it holds none.
pub(crate) fn past_first_formats(ddl: &str) -> Option<String> {
    statements(ddl)
        .ok()?
        .iter()
        .map(|t| head(t))
        .find(|(kind, _)| kind == "CREATE SCHEMA" || kind.starts_with("DROP "))
        .map(|(kind, object)| format!("{kind} {object}"))
}

/// The DDL a snail of format 7 or 8 keeps, as 0.1.x kept it: no CREATE SCHEMA, which 0.1.x does not
/// read, each schema then named by what lies in it, as 0.1.x named one. Refused for a schema nothing
/// lies in, which such a file cannot hold. Each CREATE SCHEMA written on a line of its own goes with
/// its line, the rest of the DDL as written; one written beside another statement, the statements
/// written again.
pub(crate) fn as_first_formats(ddl: &str, format: u32) -> Result<String> {
    let all = statements(ddl)?;
    let made: Vec<String> = all
        .iter()
        .map(|t| head(t))
        .filter(|(kind, _)| kind == "CREATE SCHEMA")
        .map(|(_, object)| object)
        .collect();
    if made.is_empty() {
        return Ok(ddl.to_string());
    }
    for s in &made {
        let holds = all.iter().any(|t| {
            let (kind, object) = head(t);
            let name = match kind.ends_with("INDEX") {
                true => index_on(&written(t)).unwrap_or_default(),
                false => object,
            };
            kind != "CREATE SCHEMA" && name.rsplit_once('.').is_some_and(|(x, _)| x == s)
        });
        if !holds {
            return Err(Error::new(format!(
                "the schema {s} holds nothing, and a snail of format {format} holds a schema only by \
                 what lies in it: write it in format {}",
                crate::file::FORMAT
            )));
        }
    }
    let alone = |l: &str| {
        let t = l.trim();
        t.len() > 14
            && t.is_char_boundary(14)
            && t[..14].eq_ignore_ascii_case("CREATE SCHEMA ")
            && t.ends_with(';')
            && t.matches(';').count() == 1
    };
    let text: String = ddl.split_inclusive('\n').filter(|l| !alone(l)).collect();
    if statements(&text)?.iter().all(|t| head(t).0 != "CREATE SCHEMA") {
        return Ok(text);
    }
    Ok(all
        .iter()
        .filter(|t| head(t).0 != "CREATE SCHEMA")
        .map(|t| format!("{};", written(t)))
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Whether an ALTER TABLE says IF EXISTS.
fn alters_if_exists(tokens: &[Token]) -> bool {
    let mut c = Cursor::new(tokens);
    c.kws(&["ALTER", "TABLE"]) && c.kws(&["IF", "EXISTS"])
}

/// The statements of a DDL that declare its functions, as they are written.
pub(crate) fn function_statements(ddl: &str) -> Result<Vec<String>> {
    Ok(statements(ddl)?
        .iter()
        .filter(|t| {
            matches!(
                head(t).0.as_str(),
                "CREATE FUNCTION" | "CREATE OR REPLACE FUNCTION"
            )
        })
        .map(|t| format!("{};", written(t)))
        .collect())
}

/// The statements that declare a table, as they are written: the table, its indexes and its order,
/// with every function, type, domain and collation of the DDL, and the sequences its columns take
/// values from. Where the DDL drops anything, every statement of it: a statement after a DROP may
/// stand on what the drop took, and only the whole reads to the catalog.
pub(crate) fn ddl_of(catalog: &Catalog, table: &str) -> Result<String> {
    let t = catalog
        .table(table)
        .ok_or_else(|| Error::new(format!("{table} is not in the snail")))?;
    let all = statements(&catalog.ddl)?;
    if all.iter().any(|s| head(s).0.starts_with("DROP")) {
        return Ok(with_schemas(&all, &all));
    }
    let mut sequences: Vec<String> = catalog
        .sequences
        .iter()
        .filter(|s| s.owned_by.as_ref().is_some_and(|(o, _)| o == table))
        .map(|s| s.name.clone())
        .collect();
    let draft = Draft {
        sequences: catalog.sequences.clone(),
        ..Draft::default()
    };
    for c in &t.columns {
        if let Some(e) = &c.default {
            each_expr(e, &mut |x| {
                if let Expr::Function(f) = x {
                    if object_name(&f.name).to_lowercase().ends_with("nextval") {
                        if let FunctionArguments::List(l) = &f.args {
                            if let Some(FunctionArg::Unnamed(FunctionArgExpr::Expr(a))) =
                                l.args.first()
                            {
                                if let Ok(n) = draft.nextval_of(a) {
                                    if !sequences.contains(&n) {
                                        sequences.push(n);
                                    }
                                }
                            }
                        }
                    }
                }
            });
        }
    }
    let mut out = Vec::new();
    for tokens in statements(&catalog.ddl)? {
        let (kind, object) = head(&tokens);
        let keep = match kind.as_str() {
            "CREATE FUNCTION"
            | "CREATE OR REPLACE FUNCTION"
            | "CREATE TYPE"
            | "ALTER TYPE"
            | "CREATE DOMAIN"
            | "ALTER DOMAIN"
            | "CREATE COLLATION" => true,
            "CREATE SEQUENCE" | "ALTER SEQUENCE" => sequences.contains(&object),
            "CREATE TABLE" => object == table,
            "CREATE INDEX" | "CREATE UNIQUE INDEX" => {
                index_on(&written(&tokens)).as_deref() == Some(table)
            }
            "ALTER TABLE" => object == table,
            _ => false,
        };
        if keep {
            out.push(tokens);
        }
    }
    Ok(with_schemas(&out, &all))
}

/// Statements of a DDL handed out of a snail, each schema their objects lie in made first, as
/// PostgreSQL needs: by the DDL's own CREATE SCHEMA as written where it has one, and otherwise by
/// one of its own, where a file's kept statements name a schema no statement made.
fn with_schemas(kept: &[Vec<Token>], all: &[Vec<Token>]) -> String {
    let made = |s: &str| {
        all.iter().find(|t| {
            let (kind, object) = head(t);
            kind == "CREATE SCHEMA" && object == s
        })
    };
    let mut schemas: Vec<String> = Vec::new();
    for t in kept {
        let (kind, object) = head(t);
        let name = match kind.ends_with("INDEX") {
            true => index_on(&written(t)).unwrap_or_default(),
            false => object,
        };
        if kind != "CREATE SCHEMA" && kind != "DROP SCHEMA" {
            if let Some((s, _)) = name.rsplit_once('.') {
                if !matches!(s, "public" | "pg_catalog") && !schemas.iter().any(|x| x == s) {
                    schemas.push(s.to_string());
                }
            }
        }
    }
    let mut out: Vec<String> = Vec::new();
    for s in &schemas {
        // A schema the kept statements make where they stand needs none ahead of them.
        if kept.iter().any(|t| head(t) == ("CREATE SCHEMA".to_string(), s.clone())) {
            continue;
        }
        out.push(match made(s) {
            Some(t) => format!("{};", written(t)),
            None => format!("CREATE SCHEMA {};", crate::read::calls::quote_ident(s)),
        });
    }
    out.extend(kept.iter().map(|t| format!("{};", written(t))));
    out.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    use sqlparser::tokenizer::Tokenizer;

    fn tokens(s: &str) -> Vec<Token> {
        Tokenizer::new(&PostgreSqlDialect {}, s).tokenize().unwrap()
    }

    fn form(s: &str) -> Result<Form> {
        read(&tokens(s)).expect("a form the snail reads")
    }

    #[test]
    fn a_name_taken_in_a_schema_is_refused_again_as_postgresql_refuses_it() {
        // Each statement as PostgreSQL 18.6 took or refused it, the colliding names among them.
        let read: [(&str, bool); 23] = [
            ("CREATE TABLE s.a (x bigint PRIMARY KEY)", true),
            ("CREATE TABLE s.a_pkey (x bigint PRIMARY KEY)", false),
            ("CREATE SEQUENCE s.q", true),
            ("CREATE SEQUENCE s.q", false),
            ("CREATE TABLE s.b (id bigint PRIMARY KEY, CONSTRAINT a_pkey UNIQUE (id))", false),
            ("CREATE TABLE s.b (id serial PRIMARY KEY, v bigint REFERENCES s.nowhere (x))", false),
            ("CREATE TABLE s.b (id serial PRIMARY KEY)", true),
            ("CREATE SEQUENCE s.b_id_seq", false),
            ("CREATE TABLE s.c (id serial PRIMARY KEY, w serial)", true),
            ("CREATE VIEW s.v AS SELECT x FROM s.a", true),
            ("CREATE TABLE s.v (x bigint PRIMARY KEY)", false),
            ("CREATE INDEX a_x ON s.a (x)", true),
            ("CREATE TABLE s.a_x (y bigint PRIMARY KEY)", false),
            ("CREATE TABLE s.k (x bigint, y bigint, PRIMARY KEY (x), UNIQUE (y), UNIQUE (x, y))", true),
            ("CREATE TABLE s.k_y_key (x bigint PRIMARY KEY)", false),
            ("CREATE DOMAIN s.d AS integer CONSTRAINT d_pos CHECK (VALUE > 0)", true),
            ("ALTER DOMAIN s.d RENAME CONSTRAINT d_pos TO d_big", true),
            ("ALTER DOMAIN s.d ADD CONSTRAINT d_pos CHECK (VALUE < 100)", true),
            ("ALTER DOMAIN s.d DROP CONSTRAINT d_big", true),
            ("ALTER DOMAIN s.d ADD CONSTRAINT d_big CHECK (VALUE <> 5)", true),
            ("ALTER DOMAIN s.d ADD CONSTRAINT d_pos CHECK (VALUE <> 6)", false),
            ("CREATE TABLE s.e (x bigint PRIMARY KEY, CHECK (x > 0), CHECK (x < 9))", true),
            ("CREATE TABLE s.f (x bigint PRIMARY KEY REFERENCES s.e (x), y bigint REFERENCES s.e (x))", true),
        ];
        let ddl: String = read.iter().map(|(s, _)| format!("{s};\n")).collect();
        let reading = read_ddl(&format!("CREATE SCHEMA s;\n{ddl}"), false).unwrap();
        // The schema's statement first, then the ones the test is about.
        let taken: Vec<bool> =
            reading.statements.iter().skip(1).map(|s| s.refused.is_none()).collect();
        assert_eq!(taken, read.iter().map(|r| r.1).collect::<Vec<_>>(), "{:#?}", reading.statements);
        // A key written twice is made once, under the name written where the other's was chosen.
        let reading = read_ddl(
            "CREATE SCHEMA s; CREATE TABLE s.t (id bigint PRIMARY KEY, CONSTRAINT t_named UNIQUE (id));\n\
             CREATE TABLE s.t_pkey (x bigint PRIMARY KEY);\n\
             CREATE TABLE s.u (id bigint, CONSTRAINT u_first PRIMARY KEY (id), CONSTRAINT u_second UNIQUE (id));\n\
             CREATE TABLE s.u_second (x bigint PRIMARY KEY);\n",
            false,
        )
        .unwrap();
        let refused: Vec<bool> = reading.statements.iter().map(|s| s.refused.is_some()).collect();
        assert_eq!(refused, [false, false, false, false, false]);
        let names = |t: &str| -> Vec<String> {
            let t = reading.catalog.tables.iter().find(|x| x.name == t).unwrap();
            t.orders.iter().map(|o| o.name.clone()).collect()
        };
        assert_eq!(names("s.t"), ["t_named"]);
        assert_eq!(names("s.u"), ["u_first"]);
    }

    #[test]
    fn an_expression_stops_where_a_domain_s_next_clause_begins() {
        let mut c = Cursor::new(&tokens("1 NOT NULL CHECK (VALUE > 0)"));
        assert_eq!(c.expr_until(&AFTER_DEFAULT).unwrap().to_string(), "1");
        assert!(c.kws(&["NOT", "NULL"]));
        let mut c = Cursor::new(&tokens("(1 + 2) * 3 CONSTRAINT x"));
        assert_eq!(
            c.expr_until(&AFTER_DEFAULT).unwrap().to_string(),
            "(1 + 2) * 3"
        );
        assert!(c.is("CONSTRAINT"));
    }

    #[test]
    fn a_domain_is_read_whole_where_sqlparser_drops_its_clauses() {
        let Form::CreateDomain(d) =
            form("CREATE DOMAIN s.q AS integer NOT NULL DEFAULT 1 CHECK (VALUE > 0)").unwrap()
        else {
            panic!()
        };
        assert_eq!((d.base.as_str(), d.not_null), ("integer", true));
        assert_eq!(d.default.unwrap().to_string(), "1");
        assert_eq!(d.checks[0].check.to_string(), "VALUE > 0");
        let Form::CreateDomain(d) =
            form("CREATE DOMAIN s.e text NOT NULL CHECK (position('@' in VALUE) > 1)").unwrap()
        else {
            panic!()
        };
        assert_eq!((d.base.as_str(), d.not_null), ("text", true));
        let Form::CreateDomain(d) =
            form("CREATE DOMAIN s.p AS smallint CHECK (VALUE BETWEEN 0 AND 100) DEFAULT 0")
                .unwrap()
        else {
            panic!()
        };
        assert_eq!(d.default.unwrap().to_string(), "0");
        let Form::CreateDomain(d) = form("CREATE DOMAIN s.f AS text COLLATE s.ci").unwrap() else {
            panic!()
        };
        assert_eq!(d.collation.as_deref(), Some("s.ci"));
        for (sql, why) in [
            ("CREATE DOMAIN s.x AS int NULL NOT NULL", "conflicting"),
            (
                "CREATE DOMAIN s.x AS int DEFAULT 1 DEFAULT 2",
                "multiple default",
            ),
            ("CREATE DOMAIN s.x AS int UNIQUE", "UNIQUE is not read"),
        ] {
            let e = form(sql).err().unwrap().to_string();
            assert!(e.contains(why), "{sql}: {e}");
        }
    }

    #[test]
    fn a_range_names_its_multirange_as_postgresql_does() {
        assert_eq!(multirange_name("s.floatrange"), "s.floatmultirange");
        assert_eq!(multirange_name("s.timeslot"), "s.timeslot_multirange");
        assert_eq!(multirange_name("rangeish"), "multirangeish");
        let Form::CreateType(t) = form(
            "CREATE TYPE s.c AS RANGE (SUBTYPE = text, SUBTYPE_OPCLASS = text_pattern_ops, \
             COLLATION = \"C\", MULTIRANGE_TYPE_NAME = codes)",
        )
        .unwrap() else {
            panic!()
        };
        let TypeKind::Range(r) = t.kind else { panic!() };
        assert_eq!(
            (
                r.subtype.as_str(),
                r.multirange.as_str(),
                r.collation.as_deref()
            ),
            ("text", "s.codes", Some("C"))
        );
        let Form::CreateType(t) =
            form("CREATE TYPE s.ts AS RANGE (SUBTYPE = timestamp with time zone)").unwrap()
        else {
            panic!()
        };
        let TypeKind::Range(r) = t.kind else { panic!() };
        assert_eq!(r.subtype, "timestamp with time zone");
    }

    #[test]
    fn an_identity_and_a_generation_are_taken_out_of_a_create_table() {
        let (kept, extras, _, _) = create_table_extras(&tokens(
            "CREATE TABLE s.t (id bigint GENERATED BY DEFAULT AS IDENTITY (START WITH 10 \
             INCREMENT BY 5), q int NOT NULL, n numeric(12,2) GENERATED ALWAYS AS (q * 2) VIRTUAL, \
             s int GENERATED ALWAYS AS (q + 1) STORED, u int UNIQUE NULLS NOT DISTINCT, \
             PRIMARY KEY (id))",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            text(&kept),
            "CREATE TABLE s . t ( id bigint , q int NOT NULL , n numeric ( 12 , 2 ) , s int , u int \
             UNIQUE , PRIMARY KEY ( id ) )"
        );
        let (always, options) = extras[0].1.identity.as_ref().unwrap();
        assert!(!always);
        assert!(matches!(
            options[..],
            [SequenceOption::Start(10), SequenceOption::Increment(5)]
        ));
        let g = extras[2].1.generated.as_ref().unwrap();
        assert_eq!((g.expr.to_string().as_str(), g.stored), ("q * 2", false));
        assert!(extras[3].1.generated.as_ref().unwrap().stored);
        assert_eq!(extras[4].1.unique_nulls_distinct, Some(false));
    }

    #[test]
    fn without_overlaps_is_taken_out_of_a_key_and_its_key_counted() {
        let (kept, _, _, temporal) = create_table_extras(&tokens(
            "CREATE TABLE s.r (room int4range, valid daterange, UNIQUE (room), CHECK (true), \
             CONSTRAINT r_k UNIQUE NULLS NOT DISTINCT (room, valid WITHOUT OVERLAPS), \
             PRIMARY KEY (room, valid WITHOUT OVERLAPS) INCLUDE (room))",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            text(&kept),
            "CREATE TABLE s . r ( room int4range , valid daterange , UNIQUE ( room ) , CHECK ( \
             true ) , CONSTRAINT r_k UNIQUE NULLS NOT DISTINCT ( room , valid ) , PRIMARY KEY ( \
             room , valid ) INCLUDE ( room ) )"
        );
        assert_eq!(temporal, [1, 2]);
        for ddl in [
            "CREATE TABLE s.r (room int4range, valid daterange, UNIQUE (valid WITHOUT OVERLAPS, room))",
            "CREATE TABLE s.r (room int4range, valid daterange, CHECK (valid WITHOUT OVERLAPS))",
        ] {
            let e = create_table_extras(&tokens(ddl)).err().unwrap().to_string();
            assert!(e.contains("after the last column"), "{ddl}: {e}");
        }
    }

    #[test]
    fn a_foreign_key_is_taken_out_of_a_create_table_whole_in_the_order_written() {
        let (kept, _, keys, _) = create_table_extras(&tokens(
            "CREATE TABLE s.t (id int PRIMARY KEY, a int NOT NULL CONSTRAINT ta REFERENCES s.u \
             MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL (a) DEFERRABLE INITIALLY DEFERRED \
             DEFAULT 1, CONSTRAINT tb FOREIGN KEY (b, a) REFERENCES s.v (x, y) NOT ENFORCED NOT \
             VALID, b int REFERENCES s.v (x))",
        ))
        .unwrap()
        .unwrap();
        assert_eq!(
            text(&kept),
            "CREATE TABLE s . t ( id int PRIMARY KEY , a int NOT NULL DEFAULT 1 , b int )"
        );
        let read: Vec<_> = keys
            .iter()
            .map(|k| {
                (
                    k.name.as_deref(),
                    k.columns.join(","),
                    k.parent.as_str(),
                    k.parent_columns.as_ref().map(|c| c.join(",")),
                    k.full,
                    k.on_delete.clone(),
                    k.on_update.clone(),
                    (k.deferrable, k.deferred, k.enforced, k.not_valid),
                )
            })
            .collect();
        assert_eq!(
            read[0],
            (
                Some("ta"),
                "a".into(),
                "s.u",
                None,
                true,
                ('n', vec!["a".to_string()]),
                ('c', vec![]),
                (true, true, true, false)
            )
        );
        assert_eq!(read[1].0, Some("tb"));
        assert_eq!(
            (read[1].1.as_str(), read[1].7),
            ("b,a", (false, false, false, true))
        );
        assert_eq!((read[2].0, read[2].3.as_deref()), (None, Some("x")));
        let (_, _, keys, _) = create_table_extras(&tokens(
            "CREATE TABLE s.t (a int, r int4range, period int, FOREIGN KEY (a, PERIOD r) \
             REFERENCES s.u (b, PERIOD q), FOREIGN KEY (a, period) REFERENCES s.v)",
        ))
        .unwrap()
        .unwrap();
        let read: Vec<_> = keys
            .iter()
            .map(|k| (k.columns.join(","), k.period, k.parent_period))
            .collect();
        assert_eq!(
            read,
            [
                ("a,r".to_string(), true, true),
                ("a,period".to_string(), false, false)
            ]
        );
        for (ddl, why) in [
            ("CREATE TABLE s.t (a int REFERENCES s.u ON UPDATE SET DEFAULT (a))", "only supported for ON DELETE"),
            ("CREATE TABLE s.t (a int REFERENCES s.u MATCH PARTIAL)", "MATCH PARTIAL not yet implemented"),
            ("CREATE TABLE s.t (a int REFERENCES s.u NOT DEFERRABLE INITIALLY DEFERRED)", "must be DEFERRABLE"),
            ("CREATE TABLE s.t (a int REFERENCES s.u DEFERRABLE NOT DEFERRABLE)", "conflicting constraint properties"),
            ("CREATE TABLE s.t (a int, r int, FOREIGN KEY (PERIOD r) REFERENCES s.u (a, PERIOD r))", "PERIOD is read only before the last"),
            ("CREATE TABLE s.t (a int, r int, FOREIGN KEY (PERIOD a, r) REFERENCES s.u (a, PERIOD r))", "PERIOD is read only before the last"),
            ("CREATE TABLE s.t (a int, r int, FOREIGN KEY (a, PERIOD r) REFERENCES s.u (PERIOD a, r))", "PERIOD is read only before the last"),
            ("CREATE TABLE s.t (a int, FOREIGN KEY (a) REFERENCES s.u NO INHERIT)", "NO INHERIT"),
        ] {
            let e = create_table_extras(&tokens(ddl)).err().unwrap().to_string();
            assert!(e.contains(why), "{ddl}: {e}");
        }
    }

    #[test]
    fn an_operator_class_s_parameters_are_taken_out_of_a_create_index() {
        let (kept, params) = index_extras(&tokens(
            "CREATE INDEX i ON s.t USING gist (body tsvector_ops (siglen = 100), lower(x), y)",
        ))
        .unwrap();
        assert_eq!(
            text(&kept),
            "CREATE INDEX i ON s . t USING gist ( body tsvector_ops , lower ( x ) , y )"
        );
        assert_eq!(params, [(0, "siglen = 100".to_string())]);
    }

    #[test]
    fn the_order_reads_cluster_on_through_the_tokenizer() {
        assert_eq!(
            cluster_on(&tokens("ALTER TABLE \"S\".t CLUSTER ON \"Ix\"")),
            Some(("S.t".to_string(), "Ix".to_string()))
        );
        assert_eq!(
            cluster_on(&tokens("ALTER TABLE s.t ADD COLUMN x int")),
            None
        );
    }

    #[test]
    fn a_statement_as_written_reads_again_as_its_own_tokens() {
        let sql = "INSERT INTO \"Mixed\"\"Case\".t (a, \"b\"\"c\") VALUES ('it''s', E'back\\\\slash \\'quote\\'', N'n''s', '')";
        let once = tokens(sql);
        let again = tokens(&written(&once));
        assert_eq!(
            Cursor::new(&again).t,
            Cursor::new(&once).t,
            "{}",
            written(&once)
        );
        assert!(written(&once).contains("'it''s'"), "{}", written(&once));
    }
}

/// A type written as text, as sqlparser reads it: a composite's field's, a domain's base, a range's
/// subtype.
pub(crate) fn parse_type(text: &str) -> Result<DataType> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, text).tokenize()?;
    let tokens: Vec<Token> = tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let mut p = Parser::new(&crate::grammar::POSTGRES).with_tokens(tokens.clone());
    let dt = p.parse_data_type()?;
    if p.index() < tokens.len() && !matches!(tokens[p.index()], Token::EOF) {
        return Err(Error::new(format!("{text} is not one type")));
    }
    Ok(dt)
}

/// Whether a column of the type has a collation, as PostgreSQL gives one: a string type, or an array
/// of a type that has one.
fn collatable(ty: ColumnType) -> bool {
    collation_of(ty).is_some()
}

/// The collation a column of the type takes where its DDL names none: name's own, C; any other
/// string type's the database's default, and pg_node_tree's, the planner's statistics' and the BRIN
/// summaries'; an array's its element's.
fn collation_of(ty: ColumnType) -> Option<&'static str> {
    match ty {
        ColumnType::Internal(crate::textual::Internal::Gtsvector) => None,
        ColumnType::Internal(_) => Some("default"),
        ColumnType::Composed(r) => match r.def() {
            crate::composed::Composed::Array { element, .. } => collation_of(*element),
            _ => None,
        },
        ColumnType::Name => Some("C"),
        ty if ty.is_string() => Some("default"),
        _ => None,
    }
}
