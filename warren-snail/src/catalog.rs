//! The DDL: each table, its order and its indexes, as PostgreSQL's own statements declare them.
//!
//! The order is the index `ALTER TABLE … CLUSTER ON` names, or the primary key where none is named.
//! Every other index is held in an order of its own, and so is a primary key that is not the order.

use crate::{Error, Result};
use sqlparser::ast::{
    CastKind, CharacterLength, ColumnOption, CreateFunctionBody, DataType, DateTimeField,
    ExactNumberInfo, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectName,
    ObjectNamePart, SelectItem, SetExpr, Statement, TableConstraint, TimezoneInfo, Value,
};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::Parser;
use sqlparser::tokenizer::{Token, Tokenizer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    Int2,
    Int4,
    Int8,
    Timestamptz,
    /// Text, compared by its bytes, with the most characters a value may hold.
    Text(Option<u32>),
    /// Bytes, compared byte by byte.
    Bytes,
    /// A numeric of at most 18 digits, `scale` of them after the point, held as the integer of its
    /// smallest unit.
    Numeric {
        precision: u8,
        scale: u8,
    },
}

impl ColumnType {
    /// Bytes a value takes, plain, where every value takes the same.
    pub fn width(self) -> Option<usize> {
        match self {
            ColumnType::Int2 => Some(2),
            ColumnType::Int4 => Some(4),
            ColumnType::Int8 | ColumnType::Timestamptz | ColumnType::Numeric { .. } => Some(8),
            ColumnType::Text(_) | ColumnType::Bytes => None,
        }
    }

    pub fn is_text(self) -> bool {
        matches!(self, ColumnType::Text(_))
    }

    /// Whether its values are bytes of varying length, held and compared by their bytes: text, or
    /// bytea.
    pub fn is_byte_array(self) -> bool {
        matches!(self, ColumnType::Text(_) | ColumnType::Bytes)
    }

    pub fn name(self) -> String {
        match self {
            ColumnType::Int2 => "smallint".into(),
            ColumnType::Int4 => "integer".into(),
            ColumnType::Int8 => "bigint".into(),
            ColumnType::Timestamptz => "timestamptz".into(),
            ColumnType::Text(None) => "text".into(),
            ColumnType::Text(Some(n)) => format!("varchar({n})"),
            ColumnType::Bytes => "bytea".into(),
            ColumnType::Numeric { precision, scale } => format!("numeric({precision},{scale})"),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Column {
    pub name: String,
    pub ty: ColumnType,
    pub not_null: bool,
}

/// One level of an order: a column, or the month of a timestamptz column on the UTC clock.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Level {
    Column(usize),
    MonthUtc(usize),
}

impl Level {
    /// The table column the level is read from.
    pub fn column(self) -> usize {
        match self {
            Level::Column(c) | Level::MonthUtc(c) => c,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Order {
    pub name: String,
    pub levels: Vec<Level>,
    /// The table's columns held by position, in the order they are kept.
    pub columns: Vec<usize>,
    pub unique: bool,
    /// The table's own order, rather than one of its indexes.
    pub table_order: bool,
}

impl Order {
    /// Where a table column sits among the columns this order holds.
    pub fn slot(&self, column: usize) -> Option<usize> {
        self.columns.iter().position(|&c| c == column)
    }
}

#[derive(Clone, Debug)]
pub struct Table {
    pub name: String,
    pub columns: Vec<Column>,
    /// The table's order first, then its indexes.
    pub orders: Vec<Order>,
}

impl Table {
    pub fn column(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// The columns whose month any order reads: each block keeps the rows on each of their months.
    pub fn month_columns(&self) -> Vec<usize> {
        let mut out: Vec<usize> = Vec::new();
        for o in &self.orders {
            for l in &o.levels {
                if let Level::MonthUtc(c) = *l {
                    if !out.contains(&c) {
                        out.push(c);
                    }
                }
            }
        }
        out.sort_unstable();
        out
    }

    pub fn order(&self, name: &str) -> Option<&Order> {
        self.orders.iter().find(|o| o.name == name)
    }
}

/// A function written in SQL whose body is one expression over its arguments.
#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    pub args: Vec<ColumnType>,
    pub body: Expr,
}

#[derive(Clone, Debug)]
pub struct Catalog {
    /// The DDL as written, kept in the file so the file needs nothing outside itself.
    pub ddl: String,
    pub tables: Vec<Table>,
    pub functions: Vec<Function>,
}

/// What an expression reads, once the functions it calls are read through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Shape {
    Column(usize),
    /// A timestamptz column at time zone 'UTC'.
    Clock(usize),
    MonthUtc(usize),
}

pub fn object_name(name: &ObjectName) -> String {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => ident(i),
            other => other.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// An identifier as PostgreSQL keeps it: folded to lower case unless quoted.
pub fn ident(i: &sqlparser::ast::Ident) -> String {
    if i.quote_style.is_some() {
        i.value.clone()
    } else {
        i.value.to_lowercase()
    }
}

fn column_type(dt: &DataType) -> Result<ColumnType> {
    Ok(match dt {
        DataType::SmallInt(None) | DataType::Int2(None) => ColumnType::Int2,
        DataType::Int(None) | DataType::Integer(None) | DataType::Int4(None) => ColumnType::Int4,
        DataType::BigInt(None) | DataType::Int8(None) => ColumnType::Int8,
        DataType::Timestamp(None, TimezoneInfo::Tz | TimezoneInfo::WithTimeZone) => {
            ColumnType::Timestamptz
        }
        DataType::Text => ColumnType::Text(None),
        DataType::Varchar(length)
        | DataType::CharacterVarying(length)
        | DataType::CharVarying(length) => ColumnType::Text(match length {
            None => None,
            Some(CharacterLength::IntegerLength { length, unit: None }) => Some(
                u32::try_from(*length)
                    .map_err(|_| Error::new(format!("{dt}: the length is out of range")))?,
            ),
            Some(_) => {
                return Err(Error::new(format!(
                    "{dt}: PostgreSQL counts a length in characters only"
                )))
            }
        }),
        DataType::Bytea => ColumnType::Bytes,
        DataType::Numeric(info) | DataType::Decimal(info) => {
            let (p, s) = match info {
                ExactNumberInfo::PrecisionAndScale(p, s) => (*p, *s),
                ExactNumberInfo::Precision(p) => (*p, 0),
                ExactNumberInfo::None => {
                    return Err(Error::new(format!(
                        "{dt}: a numeric is held with its precision, at most 18 digits"
                    )))
                }
            };
            if !(1..=18).contains(&p) || s < 0 || s as u64 > p {
                return Err(Error::new(format!(
                    "{dt}: a numeric is held with at most 18 digits, and a scale within them"
                )));
            }
            ColumnType::Numeric {
                precision: p as u8,
                scale: s as u8,
            }
        }
        other => return Err(Error::new(format!("the type {other} is not held"))),
    })
}

/// A numeric written in decimal, as the integer of its smallest unit at `scale`, rounded half away
/// from zero as PostgreSQL rounds it; refused where it holds more than `precision` digits.
pub fn numeric_units(text: &str, precision: u8, scale: u8) -> Result<i64> {
    let wrong = || Error::new(format!("'{text}' is not a numeric({precision},{scale})"));
    let t = text.trim();
    let (neg, t) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (whole, frac) = t.split_once('.').unwrap_or((t, ""));
    if (whole.is_empty() && frac.is_empty())
        || !whole.chars().all(|c| c.is_ascii_digit())
        || !frac.chars().all(|c| c.is_ascii_digit())
    {
        return Err(wrong());
    }
    let s = scale as usize;
    let kept: String = frac.chars().chain(std::iter::repeat('0')).take(s).collect();
    let mut units: i128 = format!("{whole}{kept}")
        .trim_start_matches('0')
        .parse::<i128>()
        .unwrap_or(0);
    if frac.len() > s && frac.as_bytes()[s] >= b'5' {
        units += 1;
    }
    if units >= 10i128.pow(precision as u32) {
        return Err(Error::new(format!(
            "'{text}' has more digits than numeric({precision},{scale}) holds"
        )));
    }
    let units = if neg { -units } else { units };
    i64::try_from(units).map_err(|_| wrong())
}

/// The collations whose order is the bytes', as the snail compares text.
const BYTE_ORDER: [&str; 2] = ["C", "POSIX"];

/// The shape of an expression over one table's columns. `alias` is the name the statement gives the
/// table; `env` holds the shapes of a function's arguments while its body is read.
pub fn shape(
    expr: &Expr,
    table: &str,
    alias: Option<&str>,
    columns: &[Column],
    functions: &[Function],
    env: &[Shape],
) -> Result<Shape> {
    let column = |name: &str| -> Result<Shape> {
        columns
            .iter()
            .position(|c| c.name == name)
            .map(Shape::Column)
            .ok_or_else(|| Error::new(format!("{table} has no column {name}")))
    };
    let bare = table.rsplit('.').next().unwrap_or(table);
    match expr {
        Expr::Nested(e) => shape(e, table, alias, columns, functions, env),
        Expr::Identifier(i) => column(&ident(i)),
        Expr::CompoundIdentifier(parts) if parts.len() >= 2 => {
            let qualifier = parts[..parts.len() - 1]
                .iter()
                .map(ident)
                .collect::<Vec<_>>()
                .join(".");
            let name = ident(&parts[parts.len() - 1]);
            if Some(qualifier.as_str()) == alias || qualifier == bare || qualifier == table {
                column(&name)
            } else {
                Err(Error::new(format!(
                    "{qualifier}.{name} names no table the statement reads"
                )))
            }
        }
        Expr::Value(v) => match &v.value {
            Value::Placeholder(p) => p
                .strip_prefix('$')
                .and_then(|n| n.parse::<usize>().ok())
                .and_then(|n| env.get(n.wrapping_sub(1)).copied())
                .ok_or_else(|| Error::new(format!("{p} is no argument of the function read"))),
            other => Err(Error::new(format!("{other} is a value, not a level"))),
        },
        Expr::Function(f) => {
            let name = object_name(&f.name);
            let function = functions
                .iter()
                .find(|g| g.name == name)
                .ok_or_else(|| Error::new(format!("{name} is no function the DDL declares")))?;
            let args = match &f.args {
                FunctionArguments::List(list) => &list.args,
                _ => return Err(Error::new(format!("{name} is called without arguments"))),
            };
            if args.len() != function.args.len() {
                return Err(Error::new(format!(
                    "{name} takes {} arguments, and {} are given",
                    function.args.len(),
                    args.len()
                )));
            }
            let mut inner = Vec::with_capacity(args.len());
            for a in args {
                match a {
                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                        inner.push(shape(e, table, alias, columns, functions, env)?)
                    }
                    other => return Err(Error::new(format!("the argument {other} is not read"))),
                }
            }
            shape(&function.body, table, alias, columns, functions, &inner)
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            let zone = match time_zone.as_ref() {
                Expr::Value(v) => match &v.value {
                    Value::SingleQuotedString(z) => z.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            if zone != "UTC" {
                return Err(Error::new(format!(
                    "only the UTC clock is read, not {time_zone}"
                )));
            }
            match shape(timestamp, table, alias, columns, functions, env)? {
                Shape::Column(c) if columns[c].ty == ColumnType::Timestamptz => Ok(Shape::Clock(c)),
                _ => Err(Error::new(format!(
                    "{timestamp} AT TIME ZONE 'UTC' needs a timestamptz column"
                ))),
            }
        }
        Expr::Cast {
            kind: CastKind::DoubleColon | CastKind::Cast,
            expr,
            data_type,
            ..
        } => {
            let month = match expr.as_ref() {
                Expr::Extract {
                    field: DateTimeField::Month,
                    expr: inner,
                    ..
                } => inner,
                Expr::Nested(e) => match e.as_ref() {
                    Expr::Extract {
                        field: DateTimeField::Month,
                        expr: inner,
                        ..
                    } => inner,
                    _ => {
                        return Err(Error::new(format!(
                            "the cast of {expr} is not a level the snail reads"
                        )))
                    }
                },
                _ => {
                    return Err(Error::new(format!(
                        "the cast of {expr} is not a level the snail reads"
                    )))
                }
            };
            if !matches!(data_type, DataType::SmallInt(None) | DataType::Int2(None)) {
                return Err(Error::new(format!(
                    "a month is read as smallint, not {data_type}"
                )));
            }
            match shape(month, table, alias, columns, functions, env)? {
                Shape::Clock(c) => Ok(Shape::MonthUtc(c)),
                Shape::Column(c) if columns[c].ty == ColumnType::Timestamptz => {
                    Err(Error::new(format!(
                        "the month of {} needs a clock: extract on a timestamptz is stable, and \
                         PostgreSQL refuses it as an index expression",
                        columns[c].name
                    )))
                }
                _ => Err(Error::new(format!(
                    "the month of {month} needs a timestamptz on a clock"
                ))),
            }
        }
        Expr::Extract {
            field: DateTimeField::Month,
            ..
        } => Err(Error::new(format!(
            "{expr} is read only as ::smallint, the way the key spells it"
        ))),
        other => Err(Error::new(format!(
            "{other} is not a level the snail reads"
        ))),
    }
}

/// The level a key's expression is. A timestamptz column on the UTC clock keys as the column itself.
fn level(s: Shape) -> Level {
    match s {
        Shape::Column(c) | Shape::Clock(c) => Level::Column(c),
        Shape::MonthUtc(c) => Level::MonthUtc(c),
    }
}

struct Index {
    name: String,
    table: String,
    keys: Vec<Expr>,
    include: Vec<String>,
    unique: bool,
}

/// The statements of a DDL, split on their semicolons by sqlparser's PostgreSQL tokenizer.
fn statements(ddl: &str) -> Result<Vec<Vec<Token>>> {
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
        .filter(|s| s.iter().any(|t| !matches!(t, Token::Whitespace(_))))
        .collect())
}

/// `ALTER TABLE <table> CLUSTER ON <index>`, which sqlparser does not parse: (table, index).
fn cluster_on(tokens: &[Token]) -> Option<(String, String)> {
    let words: Vec<&Token> = tokens
        .iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect();
    let word = |t: &Token| match t {
        Token::Word(w) => Some(w.clone()),
        _ => None,
    };
    let kw = |t: &Token, k: &str| {
        word(t).is_some_and(|w| w.quote_style.is_none() && w.value.eq_ignore_ascii_case(k))
    };
    if words.len() < 6 || !kw(words[0], "ALTER") || !kw(words[1], "TABLE") {
        return None;
    }
    let n = words.len();
    if !kw(words[n - 3], "CLUSTER") || !kw(words[n - 2], "ON") {
        return None;
    }
    let index = word(words[n - 1])?;
    let mut table = Vec::new();
    for (i, t) in words[2..n - 3].iter().enumerate() {
        if i % 2 == 1 {
            if **t != Token::Period {
                return None;
            }
            continue;
        }
        let w = word(t)?;
        table.push(if w.quote_style.is_some() {
            w.value
        } else {
            w.value.to_lowercase()
        });
    }
    let index = if index.quote_style.is_some() {
        index.value
    } else {
        index.value.to_lowercase()
    };
    Some((table.join("."), index))
}

impl Catalog {
    /// The statements of the DDL that declare a table, as they are written, with every function the
    /// DDL declares, since an index may call one.
    pub fn ddl_of(&self, table: &str) -> Result<String> {
        let mut out = Vec::new();
        for tokens in statements(&self.ddl)? {
            let text: String = tokens.iter().map(|t| t.to_string()).collect();
            let keep = match cluster_on(&tokens) {
                Some((t, _)) => t == table,
                None => match Parser::new(&PostgreSqlDialect {})
                    .with_tokens(tokens)
                    .parse_statement()?
                {
                    Statement::CreateFunction(_) => true,
                    Statement::CreateTable(t) => object_name(&t.name) == table,
                    Statement::CreateIndex(i) => object_name(&i.table_name) == table,
                    _ => false,
                },
            };
            if keep {
                out.push(format!("{};", text.trim()));
            }
        }
        Ok(out.join("\n"))
    }

    pub fn parse(ddl: &str) -> Result<Catalog> {
        let mut functions = Vec::new();
        let mut creates = Vec::new();
        let mut indexes = Vec::new();
        let mut clusters: Vec<(String, String)> = Vec::new();
        for tokens in statements(ddl)? {
            if let Some(c) = cluster_on(&tokens) {
                clusters.push(c);
                continue;
            }
            let statement = Parser::new(&PostgreSqlDialect {})
                .with_tokens(tokens)
                .parse_statement()?;
            match statement {
                Statement::CreateFunction(f) => functions.push(Self::function(&f)?),
                Statement::CreateTable(t) => creates.push(t),
                Statement::CreateIndex(i) => {
                    let name = i
                        .name
                        .as_ref()
                        .and_then(|n| {
                            n.0.last().map(|p| match p {
                                ObjectNamePart::Identifier(id) => ident(id),
                                other => other.to_string(),
                            })
                        })
                        .ok_or_else(|| {
                            Error::new("an index needs a name, which CLUSTER ON names")
                        })?;
                    if i.predicate.is_some() {
                        return Err(Error::new(format!("{name}: a partial index is not held")));
                    }
                    if i.using.is_some() {
                        return Err(Error::new(format!("{name}: only B-tree indexes are read")));
                    }
                    indexes.push(Index {
                        name,
                        table: object_name(&i.table_name),
                        keys: i.columns.iter().map(|c| c.column.expr.clone()).collect(),
                        include: i.include.iter().map(ident).collect(),
                        unique: i.unique,
                    });
                }
                other => {
                    let text = other.to_string();
                    let head: String = text.chars().take(60).collect();
                    return Err(Error::new(format!("the statement {head}… is not read")));
                }
            }
        }
        let mut tables = Vec::new();
        for t in &creates {
            let name = object_name(&t.name);
            let mut columns = Vec::new();
            let mut primary: Option<Vec<String>> = None;
            for c in &t.columns {
                let ty = column_type(&c.data_type)?;
                let mut not_null = false;
                for o in &c.options {
                    match &o.option {
                        ColumnOption::NotNull => not_null = true,
                        ColumnOption::Null => {}
                        ColumnOption::PrimaryKey(_) => {
                            primary = Some(vec![ident(&c.name)]);
                            not_null = true;
                        }
                        ColumnOption::Collation(n) => {
                            let collation = object_name(n);
                            if !ty.is_text() {
                                return Err(Error::new(format!(
                                    "{name}.{}: COLLATE {collation} on a column that is not text",
                                    ident(&c.name)
                                )));
                            }
                            if !BYTE_ORDER.contains(&collation.as_str()) {
                                return Err(Error::new(format!(
                                    "{name}.{}: COLLATE {collation}: text is compared by its bytes, so only C is read",
                                    ident(&c.name)
                                )));
                            }
                        }
                        other => {
                            return Err(Error::new(format!(
                                "{name}.{}: the option {other} is not read",
                                ident(&c.name)
                            )))
                        }
                    }
                }
                columns.push(Column {
                    name: ident(&c.name),
                    ty,
                    not_null,
                });
            }
            for k in &t.constraints {
                match k {
                    TableConstraint::PrimaryKey(p) => {
                        let mut names = Vec::new();
                        for c in &p.columns {
                            match &c.column.expr {
                                Expr::Identifier(i) => names.push(ident(i)),
                                other => {
                                    return Err(Error::new(format!(
                                        "{name}: a primary key on {other} is not read"
                                    )))
                                }
                            }
                        }
                        primary = Some(names);
                    }
                    other => {
                        return Err(Error::new(format!(
                            "{name}: the constraint {other} is not read"
                        )))
                    }
                }
            }
            if let Some(p) = &primary {
                for n in p {
                    match columns.iter_mut().find(|c| &c.name == n) {
                        Some(c) => c.not_null = true,
                        None => {
                            return Err(Error::new(format!(
                                "{name} has no column {n} for its primary key"
                            )))
                        }
                    }
                }
            }
            tables.push((name, columns, primary));
        }
        let mut out = Vec::new();
        for (name, columns, primary) in tables {
            let mut index_orders = Vec::new();
            for i in indexes.iter().filter(|i| i.table == name) {
                let mut levels = Vec::new();
                for k in &i.keys {
                    let s = shape(k, &name, None, &columns, &functions, &[])
                        .map_err(|e| Error::new(format!("{}: {e}", i.name)))?;
                    levels.push(level(s));
                }
                let mut include = Vec::new();
                for c in &i.include {
                    include.push(columns.iter().position(|x| &x.name == c).ok_or_else(|| {
                        Error::new(format!("{}: {name} has no column {c}", i.name))
                    })?);
                }
                index_orders.push((i.name.clone(), levels, include, i.unique));
            }
            if let Some((t, _)) = clusters.iter().filter(|(t, _)| *t == name).nth(1) {
                return Err(Error::new(format!("{t} is clustered on two indexes")));
            }
            let clustered = clusters
                .iter()
                .find(|(t, _)| *t == name)
                .map(|(_, i)| i.clone());
            let primary_levels = primary.as_ref().map(|p| {
                p.iter()
                    .map(|n| Level::Column(columns.iter().position(|c| &c.name == n).unwrap()))
                    .collect::<Vec<_>>()
            });
            let all: Vec<usize> = (0..columns.len()).collect();
            let (order_name, order_levels, order_unique) = match &clustered {
                Some(c) => {
                    let pos = index_orders.iter().position(|s| &s.0 == c).ok_or_else(|| {
                        Error::new(format!(
                            "{name} is clustered on {c}, which is no index of it"
                        ))
                    })?;
                    // The order holds every column, so what its index includes adds nothing.
                    let s = index_orders.remove(pos);
                    let unique = s.3 || primary_levels.as_ref() == Some(&s.1);
                    (s.0, s.1, unique)
                }
                None => match &primary_levels {
                    Some(p) => (
                        format!("{}_pkey", name.rsplit('.').next().unwrap()),
                        p.clone(),
                        true,
                    ),
                    None => {
                        return Err(Error::new(format!(
                        "{name} has no order: name one with CLUSTER ON, or give it a primary key"
                    )))
                    }
                },
            };
            if clustered.is_some() {
                if let Some(p) = &primary_levels {
                    if *p != order_levels {
                        index_orders.insert(
                            0,
                            (
                                format!("{}_pkey", name.rsplit('.').next().unwrap()),
                                p.clone(),
                                vec![],
                                true,
                            ),
                        );
                    }
                }
            }
            let key: Vec<usize> = dedup(order_levels.iter().map(|l| l.column()));
            let mut orders = vec![Order {
                name: order_name,
                levels: order_levels,
                columns: all,
                unique: order_unique,
                table_order: true,
            }];
            for (sname, levels, include, unique) in index_orders {
                let held = dedup(
                    levels
                        .iter()
                        .map(|l| l.column())
                        .chain(include.iter().copied())
                        .chain(key.iter().copied()),
                );
                orders.push(Order {
                    name: sname,
                    levels,
                    columns: held,
                    unique,
                    table_order: false,
                });
            }
            out.push(Table {
                name,
                columns,
                orders,
            });
        }
        for i in &indexes {
            if !out.iter().any(|t| t.name == i.table) {
                return Err(Error::new(format!(
                    "{} is on {}, which the DDL does not create",
                    i.name, i.table
                )));
            }
        }
        for (t, _) in &clusters {
            if !out.iter().any(|x| &x.name == t) {
                return Err(Error::new(format!(
                    "CLUSTER ON names {t}, which the DDL does not create"
                )));
            }
        }
        for t in &out {
            for o in &t.orders {
                if let Some(l) = o
                    .levels
                    .iter()
                    .find(|l| t.columns[l.column()].ty == ColumnType::Bytes)
                {
                    return Err(Error::new(format!(
                        "{}: {} is bytea, and a bytea level is not cut",
                        o.name,
                        t.columns[l.column()].name
                    )));
                }
            }
        }
        Ok(Catalog {
            ddl: ddl.to_string(),
            tables: out,
            functions,
        })
    }

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
            args.push(column_type(&a.data_type).map_err(|e| Error::new(format!("{name}: {e}")))?);
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

    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(ddl: &str) -> String {
        Catalog::parse(ddl).unwrap_err().to_string()
    }

    const TABLE: &str =
        "CREATE TABLE s.t (a integer NOT NULL, at timestamptz NOT NULL, PRIMARY KEY (a));";

    #[test]
    fn the_month_without_a_clock_is_refused_as_postgresql_refuses_it() {
        let e = refused(&format!(
            "{TABLE} CREATE INDEX m ON s.t ((extract(month FROM at)::smallint));"
        ));
        assert!(e.contains("extract on a timestamptz is stable"), "{e}");
    }

    #[test]
    fn an_instant_on_the_utc_clock_keys_as_the_instant_itself() {
        let clock =
            "CREATE FUNCTION s.clock(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE \
                     AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";
        let month = "(extract(month FROM s.clock(at))::smallint)";
        let c = Catalog::parse(&format!(
            "{clock} {TABLE} CREATE INDEX i ON s.t ((s.clock(at))); \
             CREATE INDEX mi ON s.t ({month}, (s.clock(at))) INCLUDE (at, a); \
             CREATE INDEX mc ON s.t ({month}, at) INCLUDE (a);"
        ))
        .unwrap();
        let orders = &c.tables[0].orders;
        assert_eq!(orders[1].levels, [Level::Column(1)]);
        assert_eq!(orders[2].levels, [Level::MonthUtc(1), Level::Column(1)]);
        assert_eq!(orders[2].levels, orders[3].levels);
        assert_eq!(orders[2].columns, orders[3].columns);
    }

    #[test]
    fn a_table_with_no_order_is_refused() {
        let e = refused("CREATE TABLE s.t (a integer NOT NULL);");
        assert!(e.contains("has no order"), "{e}");
    }

    #[test]
    fn cluster_on_an_index_the_table_lacks_is_refused() {
        let e = refused(&format!("{TABLE} ALTER TABLE s.t CLUSTER ON nothing;"));
        assert!(e.contains("which is no index of it"), "{e}");
    }

    #[test]
    fn the_index_an_order_is_clustered_on_may_include_columns_it_holds_already() {
        let c = Catalog::parse(&format!(
            "{TABLE} CREATE INDEX o ON s.t (at, a) INCLUDE (a); ALTER TABLE s.t CLUSTER ON o;"
        ))
        .unwrap();
        let order = &c.tables[0].orders[0];
        assert_eq!(order.name, "o");
        assert_eq!(order.levels, [Level::Column(1), Level::Column(0)]);
        let mut held = order.columns.clone();
        held.sort();
        assert_eq!(held, [0, 1]);
        let e = refused(&format!(
            "{TABLE} CREATE INDEX o ON s.t (at) INCLUDE (b); ALTER TABLE s.t CLUSTER ON o;"
        ));
        assert!(e.contains("has no column b"), "{e}");
    }

    #[test]
    fn a_type_not_held_is_refused() {
        let e = refused("CREATE TABLE s.t (a double precision NOT NULL, PRIMARY KEY (a));");
        assert!(e.contains("is not held"), "{e}");
    }

    #[test]
    fn a_numeric_is_held_to_18_digits_and_bytea_never_as_a_level() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a bigint NOT NULL, k bytea, m numeric(9,2) NOT NULL, PRIMARY KEY (a));",
        )
        .unwrap();
        let types: Vec<ColumnType> = c.tables[0].columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            types,
            [
                ColumnType::Int8,
                ColumnType::Bytes,
                ColumnType::Numeric {
                    precision: 9,
                    scale: 2
                }
            ]
        );
        for (ddl, why) in [
            (
                "CREATE TABLE s.t (a numeric NOT NULL, PRIMARY KEY (a));",
                "with its precision",
            ),
            (
                "CREATE TABLE s.t (a numeric(19,0) NOT NULL, PRIMARY KEY (a));",
                "at most 18 digits",
            ),
            (
                "CREATE TABLE s.t (k bytea NOT NULL, PRIMARY KEY (k));",
                "a bytea level",
            ),
        ] {
            let e = refused(ddl);
            assert!(e.contains(why), "{ddl}: {e}");
        }
        assert_eq!(numeric_units("12.345", 9, 2).unwrap(), 1235);
        assert_eq!(numeric_units("-12.344", 9, 2).unwrap(), -1234);
        assert_eq!(numeric_units("7", 9, 2).unwrap(), 700);
        assert!(numeric_units("10000000.00", 9, 2).is_err());
    }

    #[test]
    fn text_is_read_with_its_length_cap() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b varchar(64), c character varying(255) NOT NULL, \
             d text, e varchar COLLATE \"C\", PRIMARY KEY (a));",
        )
        .unwrap();
        let types: Vec<ColumnType> = c.tables[0].columns.iter().map(|c| c.ty).collect();
        assert_eq!(
            types,
            [
                ColumnType::Int4,
                ColumnType::Text(Some(64)),
                ColumnType::Text(Some(255)),
                ColumnType::Text(None),
                ColumnType::Text(None)
            ]
        );
        assert_eq!(types[1].width(), None);
    }

    #[test]
    fn a_collation_not_ordered_by_bytes_is_refused() {
        let e = refused(
            "CREATE TABLE s.t (a integer NOT NULL, b text COLLATE \"en_US\", PRIMARY KEY (a));",
        );
        assert!(e.contains("only C is read"), "{e}");
        let e = refused("CREATE TABLE s.t (a integer NOT NULL COLLATE \"C\", PRIMARY KEY (a));");
        assert!(e.contains("not text"), "{e}");
    }

    #[test]
    fn text_keys_an_order_and_an_index() {
        let c = Catalog::parse("CREATE TABLE s.u (a integer NOT NULL, b text, PRIMARY KEY (a)); CREATE INDEX ub ON s.u (b, a);").unwrap();
        let t = &c.tables[0];
        assert_eq!(t.orders[1].name, "ub");
        assert_eq!(t.orders[1].levels, [Level::Column(1), Level::Column(0)]);
        assert_eq!(t.orders[1].columns, [1, 0]);
        let c = Catalog::parse("CREATE TABLE s.t (a text NOT NULL, PRIMARY KEY (a));").unwrap();
        assert_eq!(c.tables[0].orders[0].levels, [Level::Column(0)]);
    }

    #[test]
    fn a_clock_in_another_zone_is_refused() {
        let e = refused(&format!(
            "CREATE FUNCTION s.c(timestamptz) RETURNS timestamp LANGUAGE sql IMMUTABLE AS $$ SELECT $1 AT TIME ZONE 'Europe/Lisbon' $$; \
             {TABLE} CREATE INDEX m ON s.t ((extract(month FROM s.c(at))::smallint));"
        ));
        assert!(e.contains("only the UTC clock"), "{e}");
    }

    #[test]
    fn the_order_reads_cluster_on_through_the_tokenizer() {
        assert_eq!(
            cluster_on(
                &Tokenizer::new(
                    &PostgreSqlDialect {},
                    "ALTER TABLE \"S\".t CLUSTER ON \"Ix\""
                )
                .tokenize()
                .unwrap()
            ),
            Some(("S.t".to_string(), "Ix".to_string()))
        );
        assert_eq!(
            cluster_on(
                &Tokenizer::new(&PostgreSqlDialect {}, "ALTER TABLE s.t ADD COLUMN x int")
                    .tokenize()
                    .unwrap()
            ),
            None
        );
    }
}
