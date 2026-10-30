//! The DDL the snail reads itself, token by token, where sqlparser 0.63 reads it otherwise than it is
//! written, or not at all: CREATE TYPE and ALTER TYPE, CREATE DOMAIN and ALTER DOMAIN, CREATE
//! SEQUENCE and ALTER SEQUENCE, CREATE COLLATION; and, inside a CREATE TABLE, a column's identity, its
//! generation expression and its UNIQUE NULLS [NOT] DISTINCT, taken out before sqlparser reads the
//! rest. Each form refuses by name what it does not know.

use crate::catalog::{
    assignable, format_type, held_type, ident, level, object_name, shape, Catalog, Check,
    Collation, Column, ColumnType, Domain, DomainCheck, Fact, Function, Generated, Identity,
    KeyConstraint, Level, Order, Reading, Sequence, SequenceType, Shape, StatementRead, Table,
    Type, TypeKind, BYTE_ORDER, TEXT_OPCLASSES,
};
use crate::{Error, Result};
use sqlparser::ast::helpers::stmt_create_table::CreateTableBuilder;
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
    let mut p = Parser::new(&PostgreSqlDialect {}).with_tokens(tokens.to_vec());
    let e = p.parse_expr()?;
    if p.index() < tokens.len() {
        return Err(Error::new(format!(
            "{} is read after the expression {e}",
            text(&tokens[p.index()..])
        )));
    }
    Ok(e)
}

/// A statement the snail reads itself.
pub(crate) enum Form {
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
    None
}

fn end(c: &Cursor, what: &str) -> Result<()> {
    if c.done() {
        Ok(())
    } else {
        Err(Error::new(format!("{what}: {} is not read", c.rest())))
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
    let sequence = Sequence::made(&name, &options, &what)?;
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

/// What a column of a CREATE TABLE writes that sqlparser 0.63 does not read, taken out of its
/// tokens: its identity and the sequence's options, its generation expression, and NULLS [NOT]
/// DISTINCT on its UNIQUE.
#[derive(Default)]
pub(crate) struct ColumnExtras {
    pub identity: Option<(bool, Vec<SequenceOption>)>,
    pub generated: Option<Generated>,
    pub unique_nulls_distinct: Option<bool>,
}

/// A CREATE TABLE's tokens with what sqlparser does not read taken out, and what each column wrote.
pub(crate) type TableExtras = (Vec<Token>, Vec<(String, ColumnExtras)>);

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
    let mut kept: Vec<Token> = c.t[..open + 1].to_vec();
    for (n, element) in elements.into_iter().enumerate() {
        if n > 0 {
            kept.push(Token::Comma);
        }
        let mut e = Cursor { t: element, i: 0 };
        let first = matches!(e.peek(), Some(Token::Word(w)) if w.quote_style.is_none()
            && ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN", "EXCLUDE", "LIKE"]
                .iter()
                .any(|k| w.value.eq_ignore_ascii_case(k)));
        if first {
            if let Some(at) = e.t.windows(2).position(|w| {
                Cursor::word_is(Some(&w[0]), "WITHOUT") && Cursor::word_is(Some(&w[1]), "OVERLAPS")
            }) {
                let _ = at;
                return Err(Error::new(format!(
                    "{table}: a key WITHOUT OVERLAPS, a temporal key, is not held"
                )));
            }
            if e.t.iter().any(|t| Cursor::word_is(Some(t), "PERIOD")) {
                return Err(Error::new(format!(
                    "{table}: a foreign key on a PERIOD, a temporal key, is not held"
                )));
            }
            if e.t.iter().any(|t| Cursor::word_is(Some(t), "REFERENCES")) {
                return Err(Error::new(format!(
                    "{table}: a FOREIGN KEY is not held yet"
                )));
            }
            kept.extend(e.t);
            continue;
        }
        let column = e.ident()?;
        if e.t.iter().any(|t| Cursor::word_is(Some(t), "REFERENCES")) {
            return Err(Error::new(format!(
                "{table}.{column}: a FOREIGN KEY is not held yet"
            )));
        }
        let mut x = ColumnExtras::default();
        let mut out: Vec<Token> = e.t[..e.i].to_vec();
        while !e.done() {
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
        kept.extend(out);
        extras.push((column, x));
    }
    kept.extend_from_slice(&c.t[close - 1..]);
    Ok(Some((kept, extras)))
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

/// The statements of a DDL, split on their semicolons by sqlparser's PostgreSQL tokenizer.
pub(crate) fn statements(ddl: &str) -> Result<Vec<Vec<Token>>> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, ddl).tokenize()?;
    let mut out = vec![Vec::new()];
    for t in tokens {
        if t == Token::SemiColon {
            out.push(Vec::new());
        } else {
            out.last_mut().unwrap().push(t);
        }
    }
    Ok(out
        .into_iter()
        .filter(|s| {
            s.iter().any(|t| {
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
        })
        .collect())
}

/// A statement as written, without the comments before it.
pub(crate) fn written(tokens: &[Token]) -> String {
    let start = tokens
        .iter()
        .position(|t| !matches!(t, Token::Whitespace(_)))
        .unwrap_or(tokens.len());
    tokens[start..]
        .iter()
        .map(|t| t.to_string())
        .collect::<String>()
        .trim()
        .to_string()
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
    let back = Tokenizer::new(&PostgreSqlDialect {}, &st.to_string()).tokenize()?;
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
    let more = less(&b, &a);
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
    levels: Vec<Level>,
    include: Vec<usize>,
    unique: bool,
    nulls_first: Vec<bool>,
    nulls_distinct: bool,
    desc: Vec<bool>,
    keys: Vec<String>,
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
    tables: Vec<Declared>,
    types: Vec<Type>,
    domains: Vec<Domain>,
    sequences: Vec<Sequence>,
    collations: Vec<Collation>,
    /// The CHECKs' names, each schema-qualified, which PostgreSQL keeps unique in a schema beside
    /// the keys' names.
    check_names: Vec<String>,
}

/// A key a CREATE TABLE writes, before its name is chosen.
struct KeySpec {
    name: Option<String>,
    primary: bool,
    columns: Vec<String>,
    include: Vec<String>,
    nulls_distinct: bool,
    deferrable: bool,
}

impl Draft {
    fn table(&self, name: &str) -> Option<&Declared> {
        self.tables.iter().find(|t| t.name == name)
    }

    fn table_mut(&mut self, name: &str) -> Option<&mut Declared> {
        self.tables.iter_mut().find(|t| t.name == name)
    }

    /// Whether a relation of that name lies in the schema: a table, an index or a sequence.
    fn relation(&self, schema: &str, name: &str) -> bool {
        self.tables.iter().any(|t| {
            (schema_of(&t.name) == schema && bare(&t.name) == name)
                || (schema_of(&t.name) == schema && t.keys().any(|k| k.name == name))
        }) || self
            .sequences
            .iter()
            .any(|s| schema_of(&s.name) == schema && bare(&s.name) == name)
    }

    /// Whether a constraint of that name lies in the schema: a key's or a CHECK's.
    fn constraint(&self, schema: &str, name: &str) -> bool {
        self.check_names.contains(&qualified(schema, name))
            || self.tables.iter().any(|t| {
                schema_of(&t.name) == schema
                    && t.primary
                        .iter()
                        .chain(t.uniques.iter())
                        .any(|k| k.name == name)
            })
    }

    fn type_named(&self, name: &str) -> Option<&'static str> {
        if self.types.iter().any(|t| t.name == name) {
            return Some("type");
        }
        if self.types.iter().any(|t| match &t.kind {
            TypeKind::Range(r) => r.multirange == name,
            _ => false,
        }) {
            return Some("multirange");
        }
        if self.domains.iter().any(|d| d.name == name) {
            return Some("domain");
        }
        if self.tables.iter().any(|t| t.name == name) {
            return Some("table's row type");
        }
        None
    }

    /// A column's type as the DDL names it, refused by name where the snail does not hold it.
    fn column_type(&self, dt: &DataType, what: &str) -> Result<ColumnType> {
        if let DataType::Custom(name, _) = dt {
            let n = object_name(name);
            if let Some(t) = self.types.iter().find(|t| t.name == n) {
                let kind = match t.kind {
                    TypeKind::Enum { .. } => "an enum",
                    TypeKind::Composite { .. } => "a composite",
                    TypeKind::Range(_) => "a range",
                };
                return Err(Error::new(format!(
                    "{what}: {n}, {kind} the DDL declares, is not held as a column's type"
                )));
            }
            if self.domains.iter().any(|d| d.name == n) {
                return Err(Error::new(format!(
                    "{what}: {n}, a domain the DDL declares, is not held as a column's type"
                )));
            }
            if self.tables.iter().any(|t| t.name == n) {
                return Err(Error::new(format!(
                    "{what}: {n}, a table's row type, is not held as a column's type"
                )));
            }
            if self.type_named(&n) == Some("multirange") {
                return Err(Error::new(format!(
                    "{what}: {n}, a multirange the DDL declares, is not held as a column's type"
                )));
            }
        }
        held_type(dt).map_err(|e| Error::new(format!("{what}: {e}")))
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
        if !c.done() || !self.sequences.iter().any(|s| s.name == name) {
            return Err(Error::new(format!(
                "nextval('{text}'): the DDL declares no sequence {name}"
            )));
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
                        self.nextval_of(s)
                            .map_err(|e| Error::new(format!("{what}: {e}")))?;
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
            return Err(Error::new(format!("relation \"{name}\" already exists")));
        }
        let mut given: Vec<SequenceOption> = vec![SequenceOption::As(ty)];
        given.extend(
            options
                .iter()
                .filter(|o| !matches!(o, SequenceOption::SequenceName(_)))
                .cloned(),
        );
        let what = format!("{table}.{column}'s sequence");
        let mut s = Sequence::made(&name, &given, &what)?;
        s.owned_by = Some((table.to_string(), column.to_string()));
        s.identity = identity;
        self.sequences.push(s);
        Ok(name)
    }

    /// Reads one statement into the draft, or refuses it by name. Returns what it says of what it
    /// read and did nothing with.
    fn apply(&mut self, tokens: &[Token]) -> Result<Vec<String>> {
        if let Some((table, index)) = cluster_on(tokens) {
            let t = self.table_mut(&table).ok_or_else(|| {
                Error::new(format!(
                    "CLUSTER ON names {table}, which the DDL does not create"
                ))
            })?;
            if t.clustered.is_some() {
                return Err(Error::new(format!("{table} is clustered on two indexes")));
            }
            if !t.keys().any(|k| k.name == index) {
                return Err(Error::new(format!(
                    "{table} is clustered on {index}, which is no index of it"
                )));
            }
            t.clustered = Some(index);
            return Ok(Vec::new());
        }
        if let Some(form) = read(tokens) {
            return self.form(form?);
        }
        if let Some((kept, extras)) = create_table_extras(tokens)? {
            return self.create_table(&kept, extras);
        }
        let (kind, object) = head(tokens);
        if kind == "CREATE INDEX" || kind == "CREATE UNIQUE INDEX" {
            return self.create_index(tokens);
        }
        if kind == "ALTER TABLE" {
            return self.alter_table(tokens, &object);
        }
        let st = Parser::new(&PostgreSqlDialect {})
            .with_tokens(tokens.to_vec())
            .parse_statement()
            .map_err(|e| {
                Error::new(format!(
                    "{kind} {object}: sqlparser 0.63 reads no such statement ({e})"
                ))
            })?;
        match &st {
            Statement::CreateFunction(f) => {
                read_as_written(tokens, &st, &format!("{kind} {object}"))?;
                let f = function(f)?;
                if self.functions.iter().any(|g| g.name == f.name) {
                    return Err(Error::new(format!("function {} already exists", f.name)));
                }
                self.functions.push(f);
                Ok(Vec::new())
            }
            _ => Err(Error::new(format!(
                "the statement {kind} {object} is not read"
            ))),
        }
    }

    fn form(&mut self, form: Form) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        match form {
            Form::CreateType(t) => {
                if let Some(k) = self.type_named(&t.name) {
                    return Err(Error::new(format!(
                        "type \"{}\" already exists, a {k}",
                        t.name
                    )));
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
                    if self.type_named(&r.multirange).is_some() {
                        return Err(Error::new(format!(
                            "type \"{}\" already exists",
                            r.multirange
                        )));
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
                        labels[i] = to;
                    }
                }
            }
            Form::CreateDomain(mut d) => {
                if let Some(k) = self.type_named(&d.name) {
                    return Err(Error::new(format!(
                        "type \"{}\" already exists, a {k}",
                        d.name
                    )));
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
                    self.check_names.push(qualified(&schema, &n));
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
                        self.check_names.push(qualified(&schema, &n));
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
                                self.check_names.retain(|x| *x != q);
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
                                *x = qualified(&schema, &to);
                            }
                        }
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
                if self.relation(schema_of(&sequence.name), bare(&sequence.name)) {
                    if if_not_exists {
                        notices.push(format!(
                            "relation \"{}\" already exists, skipping",
                            sequence.name
                        ));
                        return Ok(notices);
                    }
                    return Err(Error::new(format!(
                        "relation \"{}\" already exists",
                        sequence.name
                    )));
                }
                sequence.owned_by = self.owner(&sequence.name, &options, None)?;
                self.sequences.push(sequence);
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
                s.alter(&options, &format!("ALTER SEQUENCE {name}"))?;
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
                if let Some(f) = &collation.from {
                    if !BYTE_ORDER.contains(&f.as_str())
                        && !self.collations.iter().any(|c| &c.name == f)
                    {
                        return Err(Error::new(format!(
                            "CREATE COLLATION {}: FROM {f}: only C, POSIX and the DDL's own \
                             collations of byte order are read",
                            collation.name
                        )));
                    }
                }
                if !collation.byte_order() {
                    return Err(Error::new(format!(
                        "CREATE COLLATION {}: text is compared by its bytes, so only a collation \
                         of byte order is read (libc C or POSIX, builtin C)",
                        collation.name
                    )));
                }
                self.collations.push(collation);
            }
        }
        Ok(notices)
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

    /// ALTER TABLE … ADD [CONSTRAINT n] CHECK (…) [NOT VALID], and VALIDATE CONSTRAINT; CLUSTER ON is
    /// read before. Any other form is refused by name.
    fn alter_table(&mut self, tokens: &[Token], object: &str) -> Result<Vec<String>> {
        let mut c = Cursor::new(tokens);
        c.kws(&["ALTER", "TABLE"]);
        c.kw("ONLY");
        let name = c.name()?;
        let schema = schema_of(&name).to_string();
        if self.table(&name).is_none() {
            return Err(Error::new(format!("relation \"{name}\" does not exist")));
        }
        let mut notices = Vec::new();
        if c.kws(&["VALIDATE", "CONSTRAINT"]) {
            let n = c.ident()?;
            end(&c, &format!("ALTER TABLE {name}"))?;
            let t = self.table_mut(&name).unwrap();
            let k = t.checks.iter_mut().find(|k| k.name == n).ok_or_else(|| {
                Error::new(format!(
                    "constraint \"{n}\" of relation \"{name}\" does not exist"
                ))
            })?;
            k.valid = true;
            return Ok(notices);
        }
        if !c.kw("ADD") {
            return Err(Error::new(format!(
                "ALTER TABLE {object}: only ADD CHECK, VALIDATE CONSTRAINT and CLUSTER ON are read"
            )));
        }
        let given = if c.kw("CONSTRAINT") {
            Some(c.ident()?)
        } else {
            None
        };
        if !c.kw("CHECK") {
            return Err(Error::new(format!(
                "ALTER TABLE {object} ADD {}: only a CHECK is added",
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
                        "constraint \"{n}\" for relation \"{name}\" already exists"
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
        self.check_names.push(qualified(&schema, &n));
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
    ) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        let st = Parser::new(&PostgreSqlDialect {})
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
            let a = Tokenizer::new(&PostgreSqlDialect {}, &st.to_string()).tokenize()?;
            let b = Tokenizer::new(&PostgreSqlDialect {}, &plain.to_string()).tokenize()?;
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
        if self.table(&name).is_some() || self.relation(&schema, bare(&name)) {
            if t.if_not_exists {
                notices.push(format!("relation \"{name}\" already exists, skipping"));
                return Ok(notices);
            }
            return Err(Error::new(format!("relation \"{name}\" already exists")));
        }
        if let Some(k) = self.type_named(&name) {
            return Err(Error::new(format!("type \"{name}\" already exists, a {k}")));
        }
        let mut columns: Vec<Column> = Vec::new();
        let mut keys: Vec<KeySpec> = Vec::new();
        // Each CHECK: its name where written, its expression, NOT ENFORCED, and the column it is
        // written on.
        let mut checks: Vec<(Option<String>, Expr, bool)> = Vec::new();
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
            let ty = match serial {
                Some(s) => s.column(),
                None => self.column_type(&c.data_type, &cwhat)?,
            };
            let mut col = Column {
                name: cname.clone(),
                ty,
                not_null: false,
                default: None,
                generated: None,
                identity: None,
                collation: None,
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
                        col.default = Some(e.clone());
                    }
                    ColumnOption::PrimaryKey(p) => keys.push(KeySpec {
                        name: given.or_else(|| p.name.as_ref().map(ident)),
                        primary: true,
                        columns: vec![cname.clone()],
                        include: Vec::new(),
                        nulls_distinct: true,
                        deferrable: deferrable(p.characteristics.as_ref(), &cwhat)?,
                    }),
                    ColumnOption::Unique(u) => keys.push(KeySpec {
                        name: given.or_else(|| u.name.as_ref().map(ident)),
                        primary: false,
                        columns: vec![cname.clone()],
                        include: Vec::new(),
                        nulls_distinct: x.unique_nulls_distinct.unwrap_or(true),
                        deferrable: deferrable(u.characteristics.as_ref(), &cwhat)?,
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
                            (*k.expr).clone(),
                            k.enforced != Some(false),
                        ));
                    }
                    ColumnOption::Collation(n) => {
                        let collation = object_name(n);
                        if !ty.is_text() {
                            return Err(Error::new(format!(
                                "{cwhat}: COLLATE {collation} on a column that is not text"
                            )));
                        }
                        if !BYTE_ORDER.contains(&collation.as_str())
                            && !self.collations.iter().any(|k| k.name == collation)
                        {
                            return Err(Error::new(format!(
                                "{cwhat}: COLLATE {collation}: text is compared by its bytes, so only C is read"
                            )));
                        }
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
        for k in &t.constraints {
            match k {
                TableConstraint::PrimaryKey(p) => keys.push(KeySpec {
                    name: p.name.as_ref().map(ident),
                    primary: true,
                    columns: key_columns(&p.columns, &name)?,
                    include: p.include.iter().map(ident).collect(),
                    nulls_distinct: true,
                    deferrable: deferrable(p.characteristics.as_ref(), &name)?,
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
                        (*c.expr).clone(),
                        c.enforced != Some(false),
                    ))
                }
                TableConstraint::ForeignKey(_) => {
                    return Err(Error::new(format!("{name}: a FOREIGN KEY is not held yet")))
                }
                TableConstraint::Exclude(_) => {
                    return Err(Error::new(format!(
                        "{name}: an EXCLUDE constraint is not held yet"
                    )))
                }
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
        // The sequences, then the defaults that read them.
        let mut next = self.clone();
        next.tables.push(Declared {
            name: name.clone(),
            columns: columns.clone(),
            primary: None,
            uniques: Vec::new(),
            checks: Vec::new(),
            indexes: Vec::new(),
            clustered: None,
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
        for k in keys {
            let levels: Vec<Level> = k
                .columns
                .iter()
                .map(|n| Level::Column(column_index(n).unwrap()))
                .collect();
            let include: Vec<usize> = k.include.iter().map(|n| column_index(n).unwrap()).collect();
            if let Some(prior) = made.iter_mut().find(|m| {
                m.levels == levels
                    && m.include == include
                    && m.nulls_distinct == k.nulls_distinct
                    && m.constraint.as_ref().map(|c| c.deferrable) == Some(k.deferrable)
            }) {
                if let Some(c) = prior.constraint.as_mut() {
                    c.primary |= k.primary;
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
                    let all: Vec<String> = k.columns.iter().chain(&k.include).cloned().collect();
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
            made.push(Keyed {
                name: kname,
                nulls_first: vec![false; levels.len()],
                desc: vec![false; levels.len()],
                keys: k.columns.clone(),
                opclasses: levels
                    .iter()
                    .map(|l| columns[l.column()].ty.default_opclass().to_string())
                    .collect(),
                levels,
                include,
                unique: true,
                nulls_distinct: k.nulls_distinct,
                options: Vec::new(),
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
                            "constraint \"{n}\" for relation \"{name}\" already exists"
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
            next.check_names.push(qualified(&schema, n));
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
        d.checks = table_checks;
        *self = next;
        Ok(notices)
    }

    fn create_index(&mut self, tokens: &[Token]) -> Result<Vec<String>> {
        let mut notices = Vec::new();
        let (kept, params) = index_extras(tokens)?;
        let st = Parser::new(&PostgreSqlDialect {})
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
        let t = self.table(&table_name).ok_or_else(|| {
            Error::new(format!(
                "{shown} is on {table_name}, which the DDL does not create"
            ))
        })?;
        if i.r#async || !i.index_options.is_empty() || !i.alter_options.is_empty() {
            return Err(Error::new(format!("{shown}: {st} is not read")));
        }
        if let Some(m) = &i.using {
            let m = m.to_string().to_lowercase();
            if m != "btree" {
                return Err(Error::new(format!(
                    "{shown}: USING {m} is not held yet; a B-tree is"
                )));
            }
        }
        if let Some(p) = &i.predicate {
            return Err(Error::new(format!(
                "{shown}: a partial index, WHERE {p}, is not held yet"
            )));
        }
        if let Some((n, p)) = params.first() {
            return Err(Error::new(format!(
                "{shown}: the operator class of its key {} takes no parameters ({p})",
                n + 1
            )));
        }
        let mut levels = Vec::new();
        let mut desc = Vec::new();
        let mut nulls_first = Vec::new();
        let mut opclasses = Vec::new();
        let mut keys: Vec<Expr> = Vec::new();
        for c in &i.columns {
            let s = shape(
                &c.column.expr,
                &table_name,
                None,
                &t.columns,
                &functions,
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
            };
            let op = match &c.operator_class {
                None => default.to_string(),
                Some(o) => {
                    let o = object_name(o);
                    let o = o.strip_prefix("pg_catalog.").unwrap_or(&o).to_string();
                    let text = matches!(s, Shape::Column(k) if t.columns[k].ty.is_text());
                    if o != default && !(text && TEXT_OPCLASSES.contains(&o.as_str())) {
                        return Err(Error::new(format!(
                            "{shown}: the operator class {o} is not read on {}",
                            c.column.expr
                        )));
                    }
                    o
                }
            };
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
                    Error::new(format!("{shown}: {table_name} has no column {c}"))
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
            match k.as_str() {
                "fillfactor" => {
                    let n: i64 = v.parse().map_err(|_| {
                        Error::new(format!("{shown}: fillfactor is a number, not {v}"))
                    })?;
                    if !(10..=100).contains(&n) {
                        return Err(Error::new(format!(
                            "{shown}: value {v} out of bounds for option \"fillfactor\""
                        )));
                    }
                }
                "deduplicate_items" => {}
                other => {
                    return Err(Error::new(format!(
                        "{shown}: unrecognized parameter \"{other}\""
                    )))
                }
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
        let keyed = Keyed {
            name,
            levels,
            include,
            unique: i.unique,
            nulls_first,
            nulls_distinct: i.nulls_distinct != Some(false),
            desc,
            keys: keys.iter().map(|k| k.to_string()).collect(),
            opclasses,
            options,
            constraint: None,
        };
        self.table_mut(&table_name).unwrap().indexes.push(keyed);
        Ok(notices)
    }

    /// The tables, each given its order and its indexes; a table with no order is refused.
    fn finish(&self) -> Vec<std::result::Result<Table, (String, Error)>> {
        self.tables
            .iter()
            .map(|d| finish_table(d).map_err(|e| (d.name.clone(), e)))
            .collect()
    }
}

fn finish_table(d: &Declared) -> Result<Table> {
    let name = &d.name;
    let columns = &d.columns;
    let all: Vec<usize> = (0..columns.len())
        .filter(|&c| !columns[c].is_virtual())
        .collect();
    let mut strands: Vec<Keyed> = d.uniques.iter().chain(d.indexes.iter()).cloned().collect();
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
            s.unique = s.unique || d.primary.as_ref().map(|p| &p.levels) == Some(&s.levels);
            s
        }
        None => d.primary.clone().ok_or_else(|| {
            Error::new(format!(
                "{name} has no order: name one with CLUSTER ON, or give it a primary key"
            ))
        })?,
    };
    if d.clustered.is_some() {
        if let Some(p) = &d.primary {
            if p.levels != ordered.levels {
                strands.insert(0, p.clone());
            }
        }
    }
    let key: Vec<usize> = dedup(ordered.levels.iter().map(|l| l.column()));
    let mut orders = vec![Order {
        name: ordered.name,
        levels: ordered.levels,
        columns: all,
        unique: ordered.unique,
        table_order: true,
        nulls_first: ordered.nulls_first,
        nulls_distinct: ordered.nulls_distinct,
        desc: ordered.desc,
        keys: ordered.keys,
        opclasses: ordered.opclasses,
        include: ordered.include,
        options: ordered.options,
        constraint: ordered.constraint,
        clustered: d.clustered.is_some(),
    }];
    for s in strands {
        let held = dedup(
            s.levels
                .iter()
                .map(|l| l.column())
                .chain(s.include.iter().copied())
                .chain(key.iter().copied()),
        );
        orders.push(Order {
            name: s.name,
            levels: s.levels,
            columns: held,
            unique: s.unique,
            table_order: false,
            nulls_first: s.nulls_first,
            nulls_distinct: s.nulls_distinct,
            desc: s.desc,
            keys: s.keys,
            opclasses: s.opclasses,
            include: s.include.clone(),
            options: s.options,
            constraint: s.constraint,
            clustered: false,
        });
    }
    for o in &orders {
        if let Some(l) = o
            .levels
            .iter()
            .find(|l| columns[l.column()].ty == ColumnType::Bytes)
        {
            return Err(Error::new(format!(
                "{}: {} is bytea, and a bytea level is not cut",
                o.name,
                columns[l.column()].name
            )));
        }
        // xid and cid have only a hash operator class, so PostgreSQL keys no B-tree on them.
        if let Some(l) = o.levels.iter().find(|l| !columns[l.column()].ty.ordered()) {
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
    })
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
    for t in &c.tables {
        fact("table", &t.name, "columns", t.columns.len().to_string());
        for (i, col) in t.columns.iter().enumerate() {
            let o = format!("{}.{}", t.name, col.name);
            fact("column", &o, "position", (i + 1).to_string());
            fact("column", &o, "type", col.ty.pg_name());
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
            if col.ty.is_text() {
                fact(
                    "column",
                    &o,
                    "collation",
                    col.collation.clone().unwrap_or_else(|| "default".into()),
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
        for o in &t.orders {
            let obj = format!("{}.{}", t.name, o.name);
            if let Some(k) = &o.constraint {
                let ty = if k.primary { "p" } else { "u" };
                fact("constraint", &obj, "type", ty.into());
                fact("constraint", &obj, "columns", names(&o.levels));
                fact("constraint", &obj, "deferrable", truth(k.deferrable));
                if !k.primary {
                    fact(
                        "constraint",
                        &obj,
                        "nulls_distinct",
                        truth(o.nulls_distinct),
                    );
                }
            }
            fact("index", &obj, "method", "btree".into());
            fact("index", &obj, "unique", truth(o.unique));
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
        for k in &t.checks {
            let obj = format!("{}.{}", t.name, k.name);
            fact("constraint", &obj, "type", "c".into());
            fact("constraint", &obj, "check", k.expr.to_string());
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
        | Expr::IsNotUnknown(expr) => each_expr(expr, f),
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
        _ => {}
    }
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

/// A function written in SQL: its name, its arguments' types and its body's one expression.
fn function(f: &sqlparser::ast::CreateFunction) -> Result<Function> {
    let name = object_name(&f.name);
    let language = f.language.as_ref().map(ident).unwrap_or_default();
    if language != "sql" {
        return Err(Error::new(format!("{name}: only LANGUAGE sql is read")));
    }
    let mut args = Vec::new();
    for a in f.args.as_deref().unwrap_or(&[]) {
        if a.name.is_some() {
            return Err(Error::new(format!("{name}: named arguments are not read")));
        }
        args.push(held_type(&a.data_type).map_err(|e| Error::new(format!("{name}: {e}")))?);
    }
    let text = match &f.function_body {
        Some(CreateFunctionBody::AsBeforeOptions {
            body: Expr::Value(v),
            ..
        }) => match &v.value {
            Value::DollarQuotedString(s) => s.value.clone(),
            Value::SingleQuotedString(s) => s.clone(),
            _ => String::new(),
        },
        _ => String::new(),
    };
    let parsed = Parser::parse_sql(&PostgreSqlDialect {}, &text)
        .map_err(|e| Error::new(format!("{name}: its body: {e}")))?;
    let body = match parsed.as_slice() {
        [Statement::Query(q)] => match q.body.as_ref() {
            SetExpr::Select(s)
                if s.from.is_empty() && s.selection.is_none() && s.projection.len() == 1 =>
            {
                match &s.projection[0] {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        e.clone()
                    }
                    _ => {
                        return Err(Error::new(format!(
                            "{name}: its body is not one expression"
                        )))
                    }
                }
            }
            _ => {
                return Err(Error::new(format!(
                    "{name}: its body is not one expression"
                )))
            }
        },
        _ => return Err(Error::new(format!("{name}: its body is not one SELECT"))),
    };
    Ok(Function { name, args, body })
}

/// Reads a DDL a statement at a time. `strict` stops at the first statement refused, as a file's
/// own DDL is read.
pub(crate) fn read_ddl(ddl: &str, strict: bool) -> Result<Reading> {
    let mut draft = Draft::default();
    let mut statements: Vec<StatementRead> = Vec::new();
    for tokens in statements_of(ddl)? {
        let (kind, object) = head(&tokens);
        let mut next = draft.clone();
        let read = match next.apply(&tokens) {
            Ok(notices) => {
                draft = next;
                StatementRead {
                    text: written(&tokens),
                    kind,
                    object,
                    refused: None,
                    notices,
                }
            }
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
    let mut tables = Vec::new();
    for r in draft.finish() {
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
    let taken: Vec<String> = statements
        .iter()
        .filter(|s| s.refused.is_none())
        .map(|s| format!("{};", s.text))
        .collect();
    Ok(Reading {
        catalog: Catalog {
            ddl: if taken.is_empty() {
                String::new()
            } else {
                format!("{}\n", taken.join("\n"))
            },
            tables,
            functions: draft.functions,
            types: draft.types,
            domains: draft.domains,
            sequences: draft.sequences,
            collations: draft.collations,
        },
        statements,
    })
}

fn statements_of(ddl: &str) -> Result<Vec<Vec<Token>>> {
    statements(ddl)
}

/// What a statement declares, where it declares something the catalog holds: CREATE TABLE, INDEX,
/// SEQUENCE, TYPE, DOMAIN or COLLATION, or ALTER TABLE, SEQUENCE, TYPE or DOMAIN.
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
        _ => return None,
    })
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
/// values from.
pub(crate) fn ddl_of(catalog: &Catalog, table: &str) -> Result<String> {
    let t = catalog
        .table(table)
        .ok_or_else(|| Error::new(format!("{table} is not in the snail")))?;
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
            out.push(format!("{};", written(&tokens)));
        }
    }
    Ok(out.join("\n"))
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
        let (kept, extras) = create_table_extras(&tokens(
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
        let e = create_table_extras(&tokens(
            "CREATE TABLE s.r (room int, valid int, PRIMARY KEY (room, valid WITHOUT OVERLAPS))",
        ))
        .err()
        .unwrap()
        .to_string();
        assert!(e.contains("WITHOUT OVERLAPS"), "{e}");
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
}
