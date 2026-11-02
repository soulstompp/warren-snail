//! The element strands: what a GIN index, or a GiST index on a tsvector or a tsquery, holds of a row,
//! routed there at every write; what a condition asks of them; and the round along them, the rows
//! found by their keys on the table's order, as a join's are.

use super::change::{level_key, push, rows_of, Change};
use super::pipeline::{self, Value};
use super::question::{
    Arm, Col, Cond, Countable, Interval, Key, OpCond, Question, Select, Side, TextCond,
};
use super::statement::{Datum, Row};
use super::walk::{KeySet, Want};
use super::{Blocks, Part, Price, Round, Snail};
use crate::catalog::{Catalog, Column, ColumnType, ElementKey, Level, Method, Order, Shape, Table};
use crate::composed::{Composed, Compound, Item};
use crate::places::{null_key, KeyValue};
use crate::textual::Textual;
use crate::tsearch::Asked;
use crate::values::Values;
use crate::write::Rows;
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr, Value as SqlValue};
use std::collections::{HashMap, HashSet};
use std::ops::Bound::{Excluded, Included, Unbounded};

/// The schema the strands lie in: PostgreSQL keeps names beginning `pg_` from any DDL.
const SCHEMA: &str = "pg_strand";
/// What separates the parts of a strand's name: no DDL writes it.
const PART: char = '\u{1}';

/// The name of the table holding the strand of key `n` of an index of a table.
pub(crate) fn strand_name(table: &str, index: &str, n: usize) -> String {
    let schema = table.rsplit_once('.').map_or("", |x| x.0);
    format!("{SCHEMA}.{schema}{PART}{index}{PART}{n}")
}

/// What an operator class extracts from a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Array,
    Jsonb,
    JsonbPath,
    Tsvector,
    Tsquery,
}

/// What the operators of a column of its type read, with no operator class named.
fn kind_of_type(ty: ColumnType) -> Option<Kind> {
    match ty {
        ColumnType::Textual(Textual::Jsonb) => Some(Kind::Jsonb),
        ColumnType::Textual(Textual::Tsvector) => Some(Kind::Tsvector),
        ColumnType::Textual(Textual::Tsquery) => Some(Kind::Tsquery),
        ColumnType::Composed(r) if matches!(r.def(), Composed::Array { .. }) => Some(Kind::Array),
        _ => None,
    }
}

fn kind_of(key: &ElementKey, ty: ColumnType) -> Kind {
    match (key.opclass.as_str(), ty) {
        ("jsonb_ops", _) => Kind::Jsonb,
        ("jsonb_path_ops", _) => Kind::JsonbPath,
        ("tsquery_ops", _) => Kind::Tsquery,
        ("tsvector_ops", _) => Kind::Tsvector,
        _ => Kind::Array,
    }
}

/// The type of the elements of a key.
fn element_type(kind: Kind, ty: ColumnType) -> ColumnType {
    match kind {
        Kind::Array => match ty {
            ColumnType::Composed(r) => match r.def() {
                Composed::Array { element, .. } => *element,
                _ => unreachable!("array_ops takes an array"),
            },
            _ => unreachable!("array_ops takes an array"),
        },
        Kind::Jsonb | Kind::JsonbPath => ColumnType::Bytes,
        Kind::Tsvector | Kind::Tsquery => ColumnType::Text(None),
    }
}

/// The columns of a table's order, each once: what a strand's rows end in.
fn order_columns(table: &Table) -> Vec<usize> {
    let mut out = Vec::new();
    for l in &table.orders[0].levels {
        if !out.contains(&l.column()) {
            out.push(l.column());
        }
    }
    out
}

/// The table holding the strand of a key: its element, then the table's key, cut on the element's
/// order.
pub(crate) fn strand_table(table: &Table, key: &ElementKey) -> Table {
    let source = &table.columns[key.column];
    let ty = element_type(kind_of(key, source.ty), source.ty);
    let bare = |c: &Column| Column {
        default: None,
        generated: None,
        identity: None,
        domain: None,
        ..c.clone()
    };
    let mut columns = vec![Column {
        name: format!("{PART}element"),
        ty,
        not_null: false,
        ..bare(source)
    }];
    if !ty.is_string() {
        columns[0].collation = None;
        columns[0].library = None;
    }
    columns.extend(
        order_columns(table)
            .into_iter()
            .map(|c| bare(&table.columns[c])),
    );
    let n = columns.len();
    let order = Order {
        name: key.index.clone(),
        method: Method::Btree,
        levels: (0..n).map(Level::Column).collect(),
        columns: (0..n).collect(),
        unique: false,
        table_order: true,
        nulls_first: vec![false; n],
        nulls_distinct: true,
        desc: vec![false; n],
        keys: columns.iter().map(|c| c.name.clone()).collect(),
        opclasses: columns
            .iter()
            .map(|c| c.ty.default_opclass().to_string())
            .collect(),
        include: Vec::new(),
        options: Vec::new(),
        constraint: None,
        clustered: false,
        predicate: None,
        exclusion: None,
    };
    Table {
        name: key.strand.clone(),
        columns,
        orders: vec![order],
        checks: Vec::new(),
        release: table.release,
        foreign_keys: Vec::new(),
        monetary: table.monetary,
        format: table.format,
        element_keys: Vec::new(),
        elements_of: Some(table.name.clone()),
        brins: Vec::new(),
    }
}

/// An array's element as a column of its type holds it.
fn item_datum(ty: ColumnType, item: &Item) -> Datum {
    match item {
        Item::Null => Datum::Null,
        Item::Scalar(d) => d.clone(),
        other => Datum::Compound(std::sync::Arc::new(Compound::new(ty, other.clone()))),
    }
}

/// What a value holds, as the operator class's extraction takes it, each as often as it stands. A
/// NULL holds none; an empty value that an operator can ask for, an array or a tsquery, holds a
/// NULL, its place beside the NULLs an array holds.
fn extracted(kind: Kind, ty: ColumnType, d: &Datum) -> Result<Vec<Datum>> {
    let bytes = |v: Vec<Vec<u8>>| v.into_iter().map(Datum::Bytes).collect();
    Ok(match (kind, d) {
        (Kind::Array, Datum::Compound(c)) => match &c.item {
            Item::Array { elements, .. } if elements.is_empty() => vec![Datum::Null],
            Item::Array { elements, .. } => {
                let element = element_type(kind, ty);
                elements.iter().map(|e| item_datum(element, e)).collect()
            }
            _ => Vec::new(),
        },
        (Kind::Jsonb, Datum::Text(t)) => bytes(crate::jsonb::gin_elements(t)?),
        (Kind::JsonbPath, Datum::Text(t)) => bytes(crate::jsonb::gin_path_elements(t)?),
        (Kind::Tsvector, Datum::Text(t)) => crate::tsearch::vector_words(t)
            .into_iter()
            .map(Datum::Text)
            .collect(),
        (Kind::Tsquery, Datum::Text(t)) => {
            let words = crate::tsearch::query_words(t);
            match words.is_empty() {
                true => vec![Datum::Null],
                false => words.into_iter().map(Datum::Text).collect(),
            }
        }
        _ => Vec::new(),
    })
}

/// The key of an element on a strand.
fn element_key(strand: &Table, d: &Datum) -> Result<KeyValue> {
    let column = &strand.columns[0];
    Ok(level_key(
        column,
        column.orderer()?,
        Level::Column(0),
        d,
        KeyValue::Null { first: false },
    ))
}

/// Keeps each row's elements to one of each, as `ginExtractEntries` makes them unique: the rows it
/// writes into a strand, the element first, then the table's key.
struct Elements<'a> {
    table: &'a Table,
    key: &'a ElementKey,
    strand: &'a Table,
    kind: Kind,
    order: Vec<usize>,
    predicate: Option<super::partial::Predicate>,
}

impl<'a> Elements<'a> {
    fn of(
        catalog: &Catalog,
        table: &'a Table,
        key: &'a ElementKey,
        strand: &'a Table,
    ) -> Result<Self> {
        Ok(Elements {
            table,
            key,
            strand,
            kind: kind_of(key, table.columns[key.column].ty),
            order: order_columns(table),
            predicate: key
                .predicate
                .as_deref()
                .map(|p| super::partial::Predicate::read(catalog, table, p))
                .transpose()?,
        })
    }

    fn rows(&self, row: &Row) -> Result<Vec<Row>> {
        if self
            .predicate
            .as_ref()
            .is_some_and(|p| !p.admits(self.table, row))
        {
            return Ok(Vec::new());
        }
        let ty = self.table.columns[self.key.column].ty;
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for e in extracted(self.kind, ty, &row[self.key.column])? {
            if seen.insert(element_key(self.strand, &e)?) {
                let mut r = Vec::with_capacity(1 + self.order.len());
                r.push(e);
                r.extend(self.order.iter().map(|&c| row[c].clone()));
                out.push(r);
            }
        }
        Ok(out)
    }
}

/// What a statement's changes on a table come to on the strand of a key.
fn changes_on_strand(
    catalog: &Catalog,
    table: &Table,
    key: &ElementKey,
    strand: &Table,
    changes: &[Change],
) -> Result<Vec<Change>> {
    let elements = Elements::of(catalog, table, key, strand)?;
    let mut out = Vec::new();
    for c in changes {
        match c {
            Change::Insert(n) => out.extend(elements.rows(n)?.into_iter().map(Change::Insert)),
            Change::Delete(o) => out.extend(elements.rows(o)?.into_iter().map(Change::Delete)),
            Change::Replace(o, n) => {
                let (old, new) = (elements.rows(o)?, elements.rows(n)?);
                let (was, is): (HashSet<&Row>, HashSet<&Row>) =
                    (old.iter().collect(), new.iter().collect());
                out.extend(
                    old.iter()
                        .filter(|r| !is.contains(r))
                        .cloned()
                        .map(Change::Delete),
                );
                out.extend(
                    new.iter()
                        .filter(|r| !was.contains(r))
                        .cloned()
                        .map(Change::Insert),
                );
            }
        }
    }
    Ok(out)
}

/// The rows of the strand of each key of the tables given rows: each table's rows routed there.
pub(crate) fn strands_of(
    catalog: &Catalog,
    tables: &[(String, Rows)],
) -> Result<Vec<(String, Rows)>> {
    let mut out = Vec::new();
    for (name, rows) in tables {
        let Some(table) = catalog.table(name) else {
            continue;
        };
        for key in &table.element_keys {
            let strand = catalog
                .table(&key.strand)
                .ok_or_else(|| Error::new(format!("{name}: no strand for {}", key.index)))?;
            let elements = Elements::of(catalog, table, key, strand)?;
            let mut columns: Vec<Values> =
                strand.columns.iter().map(|c| Values::new(c.ty)).collect();
            for i in 0..rows.len() {
                let row: Row = table
                    .columns
                    .iter()
                    .zip(&rows.columns)
                    .map(|(c, v)| crate::values::datum_of(v, i, c.ty))
                    .collect();
                for r in elements.rows(&row)? {
                    for (v, d) in columns.iter_mut().zip(&r) {
                        push(v, d);
                    }
                }
            }
            out.push((key.strand.clone(), Rows { columns }));
        }
    }
    Ok(out)
}

impl Snail {
    /// A statement's changes on a table written into the strand of each of its keys: on the version
    /// a transaction of one snapshot reads, its commit writing them into the newest with its rows.
    /// The blocks rewritten in order, and the orders written.
    pub(super) fn apply_to_strands(
        &mut self,
        table: &Table,
        changes: &[Change],
    ) -> Result<(u64, u64)> {
        let (mut rewritten, mut orders) = (0, 0);
        for key in &table.element_keys {
            if self.stale {
                break;
            }
            let ti = self
                .catalog
                .tables
                .iter()
                .position(|t| t.name == key.strand)
                .ok_or_else(|| {
                    Error::new(format!("{}: no strand for {}", table.name, key.index))
                })?;
            let strand = self.catalog.tables[ti].clone();
            let changes = changes_on_strand(&self.catalog, table, key, &strand, changes)?;
            if changes.is_empty() {
                continue;
            }
            if self.held {
                self.told_of_writes(ti, &strand, &changes)?;
                let mut stage = self.reader.stage();
                orders += u64::from(self.view_order(&mut stage, ti, &strand, 0, &changes)?);
            } else {
                let mut stage = self.reader.stage();
                if let Some(n) = self.apply_order(&mut stage, ti, &strand, 0, &changes)? {
                    rewritten += n;
                    orders += 1;
                }
            }
        }
        Ok((rewritten, orders))
    }
}

/// What a condition asks of a strand: every row may hold it, or the rows holding an element, or a
/// prefix of one, all or any of these.
#[derive(Clone, Debug)]
enum Need {
    Every,
    At(Datum),
    Prefix(String),
    All(Vec<Need>),
    Any(Vec<Need>),
}

impl Need {
    /// All of these: what asks nothing left out, and each `All` within read as one.
    fn all(needs: Vec<Need>) -> Need {
        let mut out = Vec::new();
        for n in needs {
            match n {
                Need::Every => {}
                Need::All(inner) => out.extend(inner),
                n => out.push(n),
            }
        }
        match out.len() {
            0 => Need::Every,
            1 => out.pop().unwrap(),
            _ => Need::All(out),
        }
    }

    /// Any of these: every row where any asks nothing.
    fn any(needs: Vec<Need>) -> Need {
        let mut out = Vec::new();
        for n in needs {
            match n {
                Need::Every => return Need::Every,
                Need::Any(inner) => out.extend(inner),
                n => out.push(n),
            }
        }
        match out.len() {
            1 => out.pop().unwrap(),
            _ => Need::Any(out),
        }
    }
}

fn need_of_asked(a: Asked) -> Need {
    match a {
        Asked::Every => Need::Every,
        Asked::Word(w) => Need::At(Datum::Text(w)),
        Asked::Prefix(w) => Need::Prefix(w),
        Asked::All(v) => Need::all(v.into_iter().map(need_of_asked).collect()),
        Asked::Any(v) => Need::any(v.into_iter().map(need_of_asked).collect()),
    }
}

fn need_of_path(a: crate::jsonpath::Asks) -> Need {
    use crate::jsonpath::Asks;
    match a {
        Asks::At(b) => Need::At(Datum::Bytes(b)),
        Asks::All(v) => Need::all(v.into_iter().map(need_of_path).collect()),
        Asks::Any(v) => Need::any(v.into_iter().map(need_of_path).collect()),
    }
}

/// What a condition, the column on the operator's left, asks of a key's strand, as the operator
/// class's `extractQuery` reads it: no row is asked of where the value holds what no row can pass.
fn need_of(kind: Kind, ty: ColumnType, op: &str, value: &Datum) -> Result<Need> {
    let at = |v: Vec<Vec<u8>>| -> Vec<Need> {
        v.into_iter().map(|b| Need::At(Datum::Bytes(b))).collect()
    };
    let texts = || -> Vec<Option<String>> {
        match value {
            Datum::Compound(c) => match &c.item {
                Item::Array { elements, .. } => elements
                    .iter()
                    .map(|e| match e {
                        Item::Scalar(Datum::Text(t)) => Some(t.clone()),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    };
    let keys = || -> Vec<Need> {
        texts()
            .into_iter()
            .flatten()
            .map(|k| Need::At(Datum::Bytes(crate::jsonb::gin_key(&k))))
            .collect()
    };
    Ok(match (kind, op, value) {
        (Kind::Array, _, Datum::Compound(c)) => {
            let Item::Array { elements, .. } = &c.item else {
                return Ok(Need::Every);
            };
            let element = element_type(kind, ty);
            let items: Vec<Datum> = elements.iter().map(|e| item_datum(element, e)).collect();
            let defined = || -> Vec<Need> {
                items
                    .iter()
                    .filter(|d| !matches!(d, Datum::Null))
                    .cloned()
                    .map(Need::At)
                    .collect()
            };
            match op {
                "@>" if items.iter().any(|d| matches!(d, Datum::Null)) => Need::any(Vec::new()),
                "@>" => Need::all(defined()),
                "&&" => Need::any(defined()),
                "<@" => {
                    let mut any = defined();
                    any.push(Need::At(Datum::Null));
                    Need::any(any)
                }
                "=" if items.is_empty() => Need::At(Datum::Null),
                "=" => Need::all(items.into_iter().map(Need::At).collect()),
                _ => Need::Every,
            }
        }
        (Kind::Jsonb, "@>", Datum::Text(t)) => Need::all(at(crate::jsonb::gin_elements(t)?)),
        (Kind::JsonbPath, "@>", Datum::Text(t)) => {
            Need::all(at(crate::jsonb::gin_path_elements(t)?))
        }
        (Kind::Jsonb, "?", Datum::Text(k)) => Need::At(Datum::Bytes(crate::jsonb::gin_key(k))),
        (Kind::Jsonb, "?|", _) => Need::any(keys()),
        (Kind::Jsonb, "?&", _) => Need::all(keys()),
        (Kind::Jsonb | Kind::JsonbPath, "@?" | "@@", Datum::Text(p)) => {
            match crate::jsonpath::gin_asks(p, op == "@?", kind == Kind::JsonbPath)? {
                Some(a) => need_of_path(a),
                None => Need::Every,
            }
        }
        (Kind::Tsvector, "@@", Datum::Text(q)) => need_of_asked(crate::tsearch::query_asks(q)),
        (Kind::Tsquery, "@>", Datum::Text(q)) => Need::all(
            crate::tsearch::query_words(q)
                .into_iter()
                .map(|w| Need::At(Datum::Text(w)))
                .collect(),
        ),
        (Kind::Tsquery, "<@", Datum::Text(q)) => {
            let mut any: Vec<Need> = crate::tsearch::query_words(q)
                .into_iter()
                .map(|w| Need::At(Datum::Text(w)))
                .collect();
            any.push(Need::At(Datum::Null));
            Need::any(any)
        }
        _ => Need::Every,
    })
}

/// Whether an element's count is the answer's: a row passes the condition exactly where it holds the
/// element.
fn closes(kind: Kind, op: &str, value: &Datum) -> bool {
    match (kind, op, value) {
        (Kind::Array, "@>" | "&&", _) => true,
        (Kind::Tsvector, "@@", Datum::Text(q)) => crate::tsearch::query_is_one_word(q),
        _ => false,
    }
}

/// Whether a key's operator class answers an operator, the column on its left.
fn answers(kind: Kind, op: &str) -> bool {
    match kind {
        Kind::Array => matches!(op, "&&" | "@>" | "<@" | "="),
        Kind::Jsonb => matches!(op, "?" | "?|" | "?&" | "@>" | "<@" | "@?" | "@@"),
        Kind::JsonbPath => matches!(op, "@>" | "@?" | "@@"),
        Kind::Tsvector => op == "@@",
        Kind::Tsquery => matches!(op, "@>" | "<@"),
    }
}

/// The type a value on the operator's right takes.
fn value_type(kind: Kind, ty: ColumnType, op: &str) -> Option<ColumnType> {
    Some(match (kind, op) {
        (Kind::Array, _) => ty,
        (Kind::Jsonb | Kind::JsonbPath, "@>" | "<@") => ty,
        (Kind::Jsonb | Kind::JsonbPath, "@?" | "@@") => ColumnType::Textual(Textual::Jsonpath),
        (Kind::Jsonb, "?") => ColumnType::Text(None),
        (Kind::Jsonb, _) => super::typed::array_of(ColumnType::Text(None)),
        (Kind::Tsvector | Kind::Tsquery, _) => ColumnType::Textual(Textual::Tsquery),
        (Kind::JsonbPath, _) => return None,
    })
}

/// Whether an expression is a value the strand may be asked for once: a literal, a cast of one, an
/// array written out, a tsquery's function of literals.
fn fixed(e: &Expr) -> bool {
    match e {
        Expr::Value(v) => !matches!(v.value, SqlValue::Placeholder(_) | SqlValue::Null),
        Expr::Cast { expr, .. } | Expr::Nested(expr) => fixed(expr),
        Expr::Array(a) => a
            .elem
            .iter()
            .all(|x| fixed(x) || matches!(x, Expr::Value(v) if matches!(v.value, SqlValue::Null))),
        Expr::Function(f) => {
            matches!(
                crate::catalog::object_name(&f.name).as_str(),
                "to_tsquery" | "plainto_tsquery" | "phraseto_tsquery" | "websearch_to_tsquery"
            ) && match &f.args {
                sqlparser::ast::FunctionArguments::List(l) => l.args.iter().all(|a| {
                    matches!(
                        a,
                        sqlparser::ast::FunctionArg::Unnamed(
                            sqlparser::ast::FunctionArgExpr::Expr(x)
                        ) if fixed(x)
                    )
                }),
                _ => false,
            }
        }
        _ => false,
    }
}

fn array_written(e: &Expr) -> bool {
    match e {
        Expr::Array(_) => true,
        Expr::Cast { expr, .. } | Expr::Nested(expr) => array_written(expr),
        _ => false,
    }
}

/// Whether a WHERE on one table names a column whose operators `element_cond` reads: where the question
/// path reads the WHERE no other way, as where two columns meet, a pipeline holds each row to it.
pub(super) fn read_a_row_at_a_time(catalog: &Catalog, q: &sqlparser::ast::Query) -> bool {
    fn names(e: &Expr, of: &[&str]) -> bool {
        match e {
            Expr::Identifier(i) => of.contains(&crate::catalog::ident(i).as_str()),
            Expr::CompoundIdentifier(p) => p
                .last()
                .is_some_and(|i| of.contains(&crate::catalog::ident(i).as_str())),
            Expr::Nested(e) | Expr::UnaryOp { expr: e, .. } => names(e, of),
            Expr::BinaryOp { left, right, .. } => names(left, of) || names(right, of),
            _ => false,
        }
    }
    let sqlparser::ast::SetExpr::Select(s) = q.body.as_ref() else {
        return false;
    };
    let ([from], Some(w)) = (s.from.as_slice(), &s.selection) else {
        return false;
    };
    let sqlparser::ast::TableFactor::Table { name, .. } = &from.relation else {
        return false;
    };
    let Some(table) = catalog.table(&crate::catalog::object_name(name)) else {
        return false;
    };
    let of: Vec<&str> = table
        .columns
        .iter()
        .filter(|c| kind_of_type(c.ty).is_some())
        .map(|c| c.name.as_str())
        .collect();
    names(w, &of)
}

/// A condition on a column an element strand keys, by an operator its operator class answers, on a
/// value of that operator's type: none where it is another.
pub(super) fn element_cond(
    table: &Table,
    c: usize,
    op: &BinaryOperator,
    lit: &Expr,
    lit_left: bool,
) -> Result<Option<OpCond>> {
    let ty = table.columns[c].ty;
    let name = op.to_string();
    let seen = if !lit_left {
        Some(name.as_str())
    } else {
        match name.as_str() {
            "@>" => Some("<@"),
            "<@" => Some("@>"),
            "&&" | "=" | "@@" => Some(name.as_str()),
            _ => None,
        }
    };
    let Some(seen) = seen else {
        return Ok(None);
    };
    // A jsonb has no `@@` with its path on the left.
    if lit_left && seen == "@@" && matches!(kind_of_type(ty), Some(Kind::Jsonb)) {
        return Ok(None);
    }
    let keyed = table
        .element_keys
        .iter()
        .filter(|k| k.column == c)
        .map(|k| kind_of(k, ty))
        .find(|&k| answers(k, seen));
    // On these types the operator is read a row at a time with no strand to ask; an array's `=` is
    // compared by its key, as it was.
    let bare = kind_of_type(ty).filter(|&k| answers(k, seen) && (k != Kind::Array || seen != "="));
    let Some(kind) = keyed.or(bare) else {
        return Ok(None);
    };
    if !fixed(lit) {
        return Ok(None);
    }
    let Some(right) = value_type(kind, ty, seen) else {
        return Ok(None);
    };
    let other = super::typed::Typing {
        columns: &[],
        release: table.release,
        types: super::typed::Types::new(),
    }
    .type_of(lit);
    match other {
        Ok(None) => {}
        Ok(Some(t)) if t == right => {}
        _ => return Ok(None),
    }
    let Some(value) = value_as(table, lit, right) else {
        return Ok(None);
    };
    let own = Box::new(Expr::Identifier(sqlparser::ast::Ident::with_quote(
        '"',
        table.columns[c].name.clone(),
    )));
    // An array written out is held to each row as the literal of its value.
    let written = match (&value, array_written(lit)) {
        (Datum::Compound(k), true) => Box::new(Expr::value(SqlValue::SingleQuotedString(
            crate::composed::text_out(right, &k.item),
        ))),
        _ => Box::new(lit.clone()),
    };
    let (left, right) = if lit_left {
        (written, own)
    } else {
        (own, written)
    };
    Ok(Some(OpCond {
        column: c,
        expr: Expr::BinaryOp {
            left,
            op: op.clone(),
            right,
        },
        name: table.columns[c].name.clone(),
        op: seen.to_string(),
        probe: None,
        value: Some(value),
        arms: None,
        beside: Vec::new(),
    }))
}

/// A value written in a condition, as a value of type `ty`; none where it reads as no such value,
/// which the row's own evaluation then says.
fn value_as(table: &Table, lit: &Expr, ty: ColumnType) -> Option<Datum> {
    let quoted = match lit {
        Expr::Value(v) => match &v.value {
            SqlValue::SingleQuotedString(s) => Some(s),
            _ => None,
        },
        _ => None,
    };
    match (quoted, ty) {
        (_, ColumnType::Composed(_)) => crate::composed::compared(ty, lit).ok(),
        (Some(s), ColumnType::Textual(t)) => crate::textual::text_in(t, s).ok().map(Datum::Text),
        (Some(s), _) => Some(Datum::Text(s.clone())),
        (None, _) => {
            let casings = pipeline::Casings::of_table(table, &[]);
            match pipeline::eval_on(lit, &[], &[], table.release, &casings).ok()? {
                Value::Composed(t, c) if t == ty => Some(Datum::Compound(c)),
                Value::Typed(t, s) if t == ty => Some(Datum::Text(s)),
                Value::Text(s) if ty.is_text() => Some(Datum::Text(s)),
                Value::Text(s) => match ty {
                    ColumnType::Textual(t) => crate::textual::text_in(t, &s).ok().map(Datum::Text),
                    _ => None,
                },
                _ => None,
            }
        }
    }
}

/// Where a condition's value lies on a strand; or a condition an order of the table answers.
#[derive(Clone, Debug)]
enum Spot {
    Key(KeyValue),
    Prefix(Vec<u8>),
    Read(Box<Side>),
}

/// One place of one strand the round reads.
#[derive(Clone, Debug)]
struct Probe {
    strand: usize,
    spot: Spot,
}

/// A place of a strand among the probes, each place once.
fn one(probes: &mut Vec<Probe>, strand: usize, spot: Spot) -> Ask {
    let same = |p: &Probe| {
        p.strand == strand
            && match (&p.spot, &spot) {
                (Spot::Key(a), Spot::Key(b)) => a == b,
                (Spot::Prefix(a), Spot::Prefix(b)) => a == b,
                _ => false,
            }
    };
    if let Some(at) = probes.iter().position(same) {
        return Ask::One(at);
    }
    probes.push(Probe { strand, spot });
    Ask::One(probes.len() - 1)
}

/// A condition's need, its elements as probes.
#[derive(Clone, Debug)]
enum Ask {
    One(usize),
    All(Vec<Ask>),
    Any(Vec<Ask>),
}

impl Ask {
    fn probes(&self, out: &mut Vec<usize>) {
        match self {
            Ask::One(p) => out.push(*p),
            Ask::All(v) | Ask::Any(v) => v.iter().for_each(|a| a.probes(out)),
        }
    }
}

/// What the counts say of an ask, before any column page: the strand's column pages from the file,
/// the fewest and the most rows it holds, and the rows the page maps estimate.
#[derive(Clone, Copy, Debug)]
struct Asking {
    pages: u64,
    rows: (u64, u64),
    estimate: f64,
}

/// A strand's read of one place: its side, and the keys it is kept to where no condition says it.
fn spot_side(ti: usize, ty: ColumnType, spot: &Spot) -> (Side, Option<KeySet>) {
    let mut side = Side {
        table: ti,
        goes_by: String::new(),
        conds: Vec::new(),
        texts: Vec::new(),
        nulls: Vec::new(),
        empty: false,
        ops: Vec::new(),
    };
    let mut kept = None;
    match spot {
        Spot::Prefix(p) => {
            let mut next = p.clone();
            while next.last() == Some(&u8::MAX) {
                next.pop();
            }
            let hi = match next.last_mut() {
                Some(l) => {
                    *l += 1;
                    Excluded(next)
                }
                None => Unbounded,
            };
            side.texts.push(TextCond::new(0, Included(p.clone()), hi));
        }
        Spot::Key(KeyValue::Int(v))
            if matches!(
                ty,
                ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Date
            ) =>
        {
            side.conds
                .push(Cond::new(Shape::Column(0), Interval::at(i128::from(*v))));
        }
        Spot::Key(KeyValue::Wide(v)) if ty == ColumnType::Uuid => {
            side.conds
                .push(Cond::new(Shape::Column(0), Interval::at(*v)));
        }
        Spot::Key(KeyValue::Text(s)) if ty.is_text() => {
            let b = s.as_bytes().to_vec();
            side.texts
                .push(TextCond::new(0, Included(b.clone()), Included(b)));
        }
        Spot::Key(KeyValue::Bytes(b)) if ty == ColumnType::Bytes => {
            side.texts
                .push(TextCond::new(0, Included(b.to_vec()), Included(b.to_vec())));
        }
        Spot::Key(k) => kept = Some(KeySet::new(vec![0], vec![vec![k.clone()]])),
        Spot::Read(_) => unreachable!("a condition on the table is read on the table"),
    }
    (side, kept)
}

/// A table's keys, kept to a condition.
fn keys_wanted<'a>(table: &'a Table, side: &'a Side) -> Want<'a> {
    Want {
        table,
        side,
        columns: order_columns(table),
        countable: None,
        keys: None,
    }
}

/// The rows of a strand, every column, as a question.
fn strand_question(strand: &Table, side: Side) -> Question {
    let select = Select::Rows(
        (0..strand.columns.len())
            .map(|c| {
                (
                    strand.columns[c].name.clone(),
                    Key::Column(Col { side: 0, column: c }),
                )
            })
            .collect(),
    );
    Question {
        sides: vec![side],
        meets: Vec::new(),
        outer: Vec::new(),
        full: false,
        after: Vec::new(),
        select,
        order: Vec::new(),
    }
}

/// What a round on a table makes of its element strands.
pub(super) enum Along {
    /// They do not come into it.
    Not,
    /// The order read, priced against them.
    Table(Part),
    /// A count an element's counts close: the question on its strand.
    Count(Box<Question>),
    /// The strands read first, the rows found by their keys.
    Strands(Box<Chosen>),
}

pub(super) struct Chosen {
    probes: Vec<Probe>,
    ask: Ask,
    asking: Asking,
    /// The table's own read, where the strands were priced against it.
    table: Option<Part>,
    /// The index or indexes it reads, as a round names them.
    index: String,
}

impl Chosen {
    /// The column pages its strands read from the file.
    pub(super) fn strand_pages(&self) -> u64 {
        self.asking.pages
    }
}

const KEYS_PAST_THE_ROOM: &str = "an element strand's keys lie past the round's room";

impl Snail {
    /// The tables' position of a strand.
    fn strand_at(&self, key: &ElementKey) -> Result<usize> {
        self.catalog
            .tables
            .iter()
            .position(|t| t.name == key.strand)
            .ok_or_else(|| Error::new(format!("no strand for {}", key.index)))
    }

    fn asking_of(&self, probe: &Probe) -> Result<Asking> {
        let strand = &self.catalog.tables[probe.strand];
        if let Spot::Read(side) = &probe.spot {
            let part = self.part(&keys_wanted(strand, side), None)?;
            return Ok(Asking {
                pages: part.plan.column_pages - part.plan.held,
                rows: part.rows,
                estimate: part.plan.rows_estimate,
            });
        }
        let (side, kept) = spot_side(probe.strand, strand.columns[0].ty, &probe.spot);
        let want = Want {
            table: strand,
            side: &side,
            columns: (0..strand.columns.len()).collect(),
            countable: None,
            keys: kept.as_ref(),
        };
        let part = self.part(&want, None)?;
        Ok(Asking {
            pages: part.plan.column_pages - part.plan.held,
            rows: part.rows,
            estimate: part.plan.rows_estimate,
        })
    }

    fn asking(&self, ask: &Ask, probes: &[Probe]) -> Result<Asking> {
        Ok(match ask {
            Ask::One(p) => self.asking_of(&probes[*p])?,
            Ask::All(v) => {
                let mut out = Asking {
                    pages: 0,
                    rows: (0, u64::MAX),
                    estimate: f64::MAX,
                };
                for a in v {
                    let x = self.asking(a, probes)?;
                    out.pages += x.pages;
                    out.rows.1 = out.rows.1.min(x.rows.1);
                    out.estimate = out.estimate.min(x.estimate);
                }
                out
            }
            Ask::Any(v) => {
                let mut out = Asking {
                    pages: 0,
                    rows: (0, 0),
                    estimate: 0.0,
                };
                for a in v {
                    let x = self.asking(a, probes)?;
                    out.pages += x.pages;
                    out.rows.0 = out.rows.0.max(x.rows.0);
                    out.rows.1 += x.rows.1;
                    out.estimate += x.estimate;
                }
                out
            }
        })
    }

    /// A need's probes on the strand of `ti`, added to those given.
    fn ask_of(&self, ti: usize, need: &Need, probes: &mut Vec<Probe>) -> Result<Ask> {
        let strand = &self.catalog.tables[ti];
        Ok(match need {
            Need::Every => unreachable!("a condition asking nothing of a strand is not read on it"),
            Need::At(d) => one(probes, ti, Spot::Key(element_key(strand, d)?)),
            Need::Prefix(w) => one(probes, ti, Spot::Prefix(w.as_bytes().to_vec())),
            Need::All(v) => Ask::All(
                v.iter()
                    .map(|n| self.ask_of(ti, n, probes))
                    .collect::<Result<_>>()?,
            ),
            Need::Any(v) => Ask::Any(
                v.iter()
                    .map(|n| self.ask_of(ti, n, probes))
                    .collect::<Result<_>>()?,
            ),
        })
    }

    /// What a condition asks of the strand reading it in the fewest pages: its members, what they
    /// come to, and the strand's index; none where no strand may answer it.
    fn best_ask(
        &self,
        want: &Want,
        o: &OpCond,
        named: Option<&str>,
        beside: &Side,
        probes: &mut Vec<Probe>,
    ) -> Result<Option<(Vec<Ask>, Asking, String)>> {
        let (table, side) = (want.table, want.side);
        let Some(value) = &o.value else {
            // A condition by an operator a strand of the table answers: its keys, read by that strand,
            // where no order holding all a question reads already answers it.
            if o.probe.is_none() {
                return Ok(None);
            }
            let served = match named {
                Some(n) => {
                    let at = table.orders.iter().position(|x| x.name == n);
                    at.is_some_and(|at| self.serving(want).is_ok_and(|s| s.contains(&at)))
                }
                None => self.part(want, None)?.plan.walk.order != 0,
            };
            if served {
                return Ok(None);
            }
            let own = Side {
                table: side.table,
                goes_by: String::new(),
                conds: Vec::new(),
                texts: Vec::new(),
                nulls: Vec::new(),
                empty: false,
                ops: vec![o.clone()],
            };
            let Some((ask, index)) = self.read_ask(want, &own, named, probes)? else {
                return Ok(None);
            };
            // The table's own order is read kept to the condition already.
            if index.is_empty() {
                return Ok(None);
            }
            let asking = self.asking(&ask, probes)?;
            return Ok(Some((vec![ask], asking, index.join("+"))));
        };
        let ty = table.columns[o.column].ty;
        let mut best: Option<(Vec<Ask>, Asking, String)> = None;
        for key in table
            .element_keys
            .iter()
            .filter(|k| k.column == o.column && named.is_none_or(|n| k.index == n))
        {
            let kind = kind_of(key, ty);
            if !answers(kind, &o.op) {
                continue;
            }
            if let Some(p) = &key.predicate {
                let p = super::partial::Predicate::read(&self.catalog, table, p)?;
                if !p.implied_by(beside) {
                    continue;
                }
            }
            let need = need_of(kind, ty, &o.op, value)?;
            if matches!(need, Need::Every) {
                continue;
            }
            let ti = self.strand_at(key)?;
            let ask = self.ask_of(ti, &need, probes)?;
            let asks = match ask {
                Ask::All(v) => v,
                a => vec![a],
            };
            let mut asking = Asking {
                pages: 0,
                rows: (0, u64::MAX),
                estimate: 0.0,
            };
            for a in &asks {
                let x = self.asking(a, probes)?;
                asking.pages += x.pages;
                asking.rows.1 = asking.rows.1.min(x.rows.1);
                asking.estimate += x.estimate;
            }
            if asks.len() == 1 {
                asking = self.asking(&asks[0], probes)?;
            }
            if best.as_ref().is_none_or(|b| asking.pages < b.1.pages) {
                best = Some((asks, asking, key.index.clone()));
            }
        }
        Ok(best)
    }

    /// What a condition on the table asks of the order that reads it in the fewest pages, and the
    /// order, where another is named none: none where that read rules no block out, so that a row
    /// could pass in every one.
    fn read_ask(
        &self,
        want: &Want,
        own: &Side,
        named: Option<&str>,
        probes: &mut Vec<Probe>,
    ) -> Result<Option<(Ask, Vec<String>)>> {
        let table = want.table;
        if own.empty {
            return Ok(Some((Ask::Any(Vec::new()), Vec::new())));
        }
        let part = self.part(&keys_wanted(table, own), None)?;
        let w = &part.plan.walk;
        let name = &table.orders[w.order].name;
        if w.statuses.0 == 0 || named.is_some_and(|n| n != name) {
            return Ok(None);
        }
        let spot = Spot::Read(Box::new(own.clone()));
        probes.push(Probe {
            strand: own.table,
            spot,
        });
        let index = match w.order {
            0 => Vec::new(),
            _ => vec![name.clone()],
        };
        Ok(Some((Ask::One(probes.len() - 1), index)))
    }

    /// What a part of an OR or a NOT asks of the strands, and the indexes it reads: none where it may
    /// pass a row that holds nothing a strand could find. `beside` holds the conditions that stand
    /// with it, for a partial strand's predicate.
    fn arm_ask(
        &self,
        want: &Want,
        arm: &Arm,
        named: Option<&str>,
        beside: &Side,
        probes: &mut Vec<Probe>,
    ) -> Result<Option<(Ask, Vec<String>)>> {
        let mut index = Vec::new();
        let mut asks = Vec::new();
        let (parts, every) = match arm {
            Arm::Held => return Ok(None),
            Arm::Read(own) => return self.read_ask(want, own, named, probes),
            Arm::Op(o) => {
                return Ok(self
                    .best_ask(want, o, named, beside, probes)?
                    .map(|(mut v, _, i)| {
                        let ask = match v.len() {
                            1 => v.remove(0),
                            _ => Ask::All(v),
                        };
                        (ask, vec![i])
                    }))
            }
            Arm::All(v) => (v, false),
            Arm::Any(v) => (v, true),
        };
        let mut with = beside.clone();
        if !every {
            for part in parts {
                if let Arm::Read(own) = part {
                    with.conds.extend(own.conds.iter().cloned());
                    with.texts.extend(own.texts.iter().cloned());
                    with.nulls.extend(own.nulls.iter().copied());
                }
            }
        }
        for part in parts {
            match self.arm_ask(want, part, named, &with, probes)? {
                Some((a, i)) => {
                    asks.push(a);
                    index.extend(i);
                }
                None if every => return Ok(None),
                None => {}
            }
        }
        index.sort_unstable();
        index.dedup();
        Ok(match (asks.len(), every) {
            (0, _) => None,
            (_, true) => Some((Ask::Any(asks), index)),
            (1, false) => Some((asks.remove(0), index)),
            (_, false) => Some((Ask::All(asks), index)),
        })
    }

    /// What a round on one table makes of its element strands: the rows found along them where that
    /// reads the fewest pages, or along `order` where it names one's index.
    pub(super) fn along_elements(
        &self,
        q: &Question,
        want: &Want,
        order: Option<&str>,
    ) -> Result<Along> {
        let table = want.table;
        let side = want.side;
        if table.element_keys.is_empty() {
            return Ok(Along::Not);
        }
        let named = order.filter(|n| table.element_keys.iter().any(|k| k.index == *n));
        if order.is_some() && named.is_none() {
            return Ok(Along::Not);
        }
        // The keys find their rows where they lead the table's order.
        let levels = &table.orders[0].levels;
        let keyed = levels.iter().all(|l| matches!(l, Level::Column(_)))
            && order_columns(table).len() == levels.len();
        let mut probes = Vec::new();
        let mut children: Vec<(Ask, Asking, String, Option<&OpCond>)> = Vec::new();
        for o in side.ops.iter().filter(|_| keyed && !side.empty) {
            if let Some(arm) = &o.arms {
                if let Some((ask, index)) = self.arm_ask(want, arm, named, side, &mut probes)? {
                    let x = self.asking(&ask, &probes)?;
                    children.push((ask, x, index.join("+"), None));
                }
                continue;
            }
            if let Some((asks, asking, index)) = self.best_ask(want, o, named, side, &mut probes)? {
                if asks.len() == 1 {
                    children.push((asks.into_iter().next().unwrap(), asking, index, Some(o)));
                } else {
                    for a in asks {
                        let x = self.asking(&a, &probes)?;
                        children.push((a, x, index.clone(), None));
                    }
                }
            }
        }
        if children.is_empty() {
            if let Some(n) = named {
                // Keys of another kind beside the elements are an order the question may read.
                if let Some(o) = table.orders.iter().position(|o| o.name == n) {
                    if self.serving(want)?.contains(&o) {
                        return Ok(Along::Not);
                    }
                }
                let columns: Vec<usize> = table
                    .element_keys
                    .iter()
                    .filter(|k| k.index == n)
                    .map(|k| k.column)
                    .collect();
                let reads = |o: &OpCond| {
                    columns.contains(&o.column) || o.beside.iter().any(|b| columns.contains(&b.0))
                };
                if !side.ops.iter().any(reads) {
                    return Err(Error::new(format!(
                        "{n} does not hold every column the question reads"
                    )));
                }
            }
            return match named {
                Some(_) => Ok(Along::Table(self.part(want, None)?)),
                None => Ok(Along::Not),
            };
        }
        children.sort_by_key(|c| c.1.pages);
        let first = match named {
            Some(_) => None,
            None => Some(self.part(want, None)?),
        };
        // A count an element's counts close, its strand's pages fewest.
        if let (
            [(Ask::One(p), _, index, Some(o @ OpCond { value: Some(_), .. }))],
            Some(Countable::Total),
        ) = (children.as_slice(), want.countable)
        {
            let lone = side.ops.len() == 1
                && side.conds.is_empty()
                && side.texts.is_empty()
                && side.nulls.is_empty();
            let ty = table.columns[o.column].ty;
            let key = table
                .element_keys
                .iter()
                .find(|k| k.index == *index && k.column == o.column);
            let exact = key.is_some_and(|k| {
                o.value
                    .as_ref()
                    .is_some_and(|v| closes(kind_of(k, ty), &o.op, v))
            });
            let probe = &probes[*p];
            let strand = &self.catalog.tables[probe.strand];
            let (spot_of, kept) = spot_side(probe.strand, strand.columns[0].ty, &probe.spot);
            let defined = !matches!(probe.spot, Spot::Key(KeyValue::Null { .. }));
            if lone && exact && defined && kept.is_none() {
                let w = Want {
                    table: strand,
                    side: &spot_of,
                    columns: Vec::new(),
                    countable: Some(Countable::Total),
                    keys: None,
                };
                let part = self.part(&w, None)?;
                let own = part.plan.column_pages - part.plan.held;
                let theirs = first
                    .as_ref()
                    .map_or(u64::MAX, |f| f.plan.column_pages - f.plan.held);
                if own <= theirs {
                    let mut asked = strand_question(strand, spot_of.clone());
                    asked.select = q.select.clone();
                    return Ok(Along::Count(Box::new(asked)));
                }
            }
        }
        // The rows' keys find them on the table's order: at most each key's own block, at least the
        // blocks the fewest rows must fill. The strand is read where what it costs, should its keys
        // lie in every block, is no more than what it saves should they lie together.
        let bounds = first.as_ref().map(|f| {
            let t = f.plan.column_pages - f.plan.held;
            (t, f.plan.block_bounds(), f.table_rows)
        });
        let mut chosen: Vec<Ask> = Vec::new();
        let mut asking = Asking {
            pages: 0,
            rows: (0, u64::MAX),
            estimate: f64::MAX,
        };
        let index = children[0].2.clone();
        for (a, x, _, _) in &children {
            // The next is read while its pages are under what the rows it leaves out could save.
            if let (false, Some((t, b, n))) = (chosen.is_empty(), &bounds) {
                let most = asking.rows.1.saturating_mul(u64::from(b.1)).min(*t);
                let out = 1.0 - (x.estimate / (*n).max(1) as f64).min(1.0);
                if x.pages as f64 >= most as f64 * out {
                    break;
                }
            }
            chosen.push(a.clone());
            asking.pages += x.pages;
            if chosen.len() == 1 {
                asking.rows = x.rows;
            } else {
                asking.rows = (0, asking.rows.1.min(x.rows.1));
            }
            asking.estimate = asking.estimate.min(x.estimate);
        }
        if let Some((t, b, _)) = &bounds {
            let fewest = asking
                .rows
                .0
                .div_ceil(u64::from(b.0.max(1)))
                .saturating_mul(u64::from(b.2));
            if asking.pages.saturating_mul(2).saturating_add(fewest) >= *t {
                return Ok(Along::Table(first.unwrap()));
            }
        }
        let ask = match chosen.len() {
            1 => chosen.pop().unwrap(),
            _ => Ask::All(chosen),
        };
        let mut used = Vec::new();
        ask.probes(&mut used);
        let index = match used
            .iter()
            .map(|&p| probes[p].strand)
            .collect::<HashSet<_>>()
            .len()
        {
            1 => index,
            _ => children
                .iter()
                .map(|c| c.2.clone())
                .collect::<Vec<_>>()
                .join("+"),
        };
        Ok(Along::Strands(Box::new(Chosen {
            probes,
            ask,
            asking,
            table: first,
            index,
        })))
    }

    /// The keys of the rows the strands say may pass, read from them, and their round; none where
    /// they pass the room, the round then reading the table's order alone.
    pub(super) fn keys_of(
        &self,
        table: &Table,
        c: &Chosen,
    ) -> Result<(Option<KeySet>, Round, crate::room::Holding)> {
        let mut used = Vec::new();
        c.ask.probes(&mut used);
        used.sort_unstable();
        used.dedup();
        let order = &table.orders[0];
        let columns = order_columns(table);
        let levels: Vec<(usize, &Column)> =
            columns.iter().map(|&k| (k, &table.columns[k])).collect();
        let collators = levels
            .iter()
            .map(|(_, col)| col.orderer())
            .collect::<Result<Vec<_>>>()?;
        let mut keys: HashMap<Vec<KeyValue>, u32> = HashMap::new();
        let mut sets: Vec<Vec<u32>> = vec![Vec::new(); c.probes.len()];
        let mut held = crate::room::holding(0);
        let mut round = Round::default();
        // Each strand read once for the elements it is asked for, a prefix on its own.
        let mut strands: Vec<usize> = used
            .iter()
            .filter(|&&p| !matches!(c.probes[p].spot, Spot::Read(_)))
            .map(|&p| c.probes[p].strand)
            .collect();
        strands.sort_unstable();
        strands.dedup();
        let mut past = false;
        // A condition on the table: the keys of its rows, from the order that reads them.
        for &p in &used {
            let Spot::Read(own) = &c.probes[p].spot else {
                continue;
            };
            let want = keys_wanted(table, own);
            let select = Select::Rows(
                columns
                    .iter()
                    .map(|&k| {
                        (
                            table.columns[k].name.clone(),
                            Key::Column(Col { side: 0, column: k }),
                        )
                    })
                    .collect(),
            );
            let q = Question {
                sides: vec![(**own).clone()],
                meets: Vec::new(),
                outer: Vec::new(),
                full: false,
                after: Vec::new(),
                select,
                order: Vec::new(),
            };
            let mut blocks = Blocks {
                table,
                q: &q,
                answer: super::answer::Answer::new(&q.select).typed(&[table]),
                most: None,
                feed: |batch: arrow::array::RecordBatch| {
                    for row in rows_of(&batch)? {
                        let key: Vec<KeyValue> = levels
                            .iter()
                            .zip(&collators)
                            .enumerate()
                            .map(|(li, ((_, col), coll))| {
                                level_key(
                                    col,
                                    *coll,
                                    order.levels[li],
                                    &row[li],
                                    null_key(order, li),
                                )
                            })
                            .collect();
                        let next = keys.len() as u32;
                        let id = match keys.get(&key) {
                            Some(&id) => id,
                            None => {
                                let bytes =
                                    key.iter().map(|k| k.to_string().len() as u64 + 32).sum();
                                if !held.fits(bytes) {
                                    return Err(Error::new(KEYS_PAST_THE_ROOM));
                                }
                                keys.insert(key, next);
                                next
                            }
                        };
                        sets[p].push(id);
                    }
                    Ok(())
                },
            };
            let pages = self.reader.pages_read();
            let (before, held_before) = (self.touched(), self.pages_held());
            let part = self.part(&want, None)?;
            let name = table.orders[part.plan.walk.order].name.clone();
            let columns_read: Vec<(usize, bool)> = columns.iter().map(|&k| (k, false)).collect();
            match self.scan_part(&want, &columns_read, part, &mut blocks, before, held_before) {
                Ok((r, _, _)) => {
                    round.add_table(&r, &name);
                    round.reads.push(r.clone());
                    round.rows_walked += r.rows_walked;
                    round.rows_taken += r.rows_taken;
                }
                Err(e) if e.0 == KEYS_PAST_THE_ROOM => {
                    self.reader.read_done();
                    let read = self.reader.pages_read() - pages;
                    let r = Round {
                        order: name,
                        column_pages_predicted: read,
                        column_pages_read: read,
                        column_pages_high: read,
                        ..Default::default()
                    };
                    round.add_table(&r, &r.order);
                    round.reads.push(r);
                    past = true;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        if past {
            return Ok((None, round, crate::room::holding(0)));
        }
        for ti in strands {
            let strand = &self.catalog.tables[ti];
            let mut at: HashMap<KeyValue, usize> = HashMap::new();
            let mut reads: Vec<(Side, Option<KeySet>, Option<usize>)> = Vec::new();
            for &p in used.iter().filter(|&&p| c.probes[p].strand == ti) {
                match &c.probes[p].spot {
                    Spot::Prefix(_) => {
                        let (side, kept) = spot_side(ti, strand.columns[0].ty, &c.probes[p].spot);
                        reads.push((side, kept, Some(p)));
                    }
                    Spot::Key(k) => {
                        at.insert(k.clone(), p);
                    }
                    Spot::Read(_) => unreachable!("a condition on the table is read on the table"),
                }
            }
            match at.len() {
                0 => {}
                1 => {
                    let (k, p) = at.iter().next().unwrap();
                    let (side, kept) = spot_side(ti, strand.columns[0].ty, &Spot::Key(k.clone()));
                    reads.push((side, kept, Some(*p)));
                }
                _ => {
                    let (side, _) = spot_side(ti, strand.columns[0].ty, &Spot::Prefix(Vec::new()));
                    let whole = Side {
                        texts: Vec::new(),
                        ..side
                    };
                    let kept = KeySet::new(
                        vec![0],
                        at.keys().map(|k| vec![k.clone()]).collect::<Vec<_>>(),
                    );
                    reads.push((whole, Some(kept), None));
                }
            }
            for (side, kept, only) in reads {
                let q = strand_question(strand, side);
                let mut blocks = Blocks {
                    table: strand,
                    q: &q,
                    answer: super::answer::Answer::new(&q.select).typed(&[strand]),
                    most: None,
                    feed: |batch: arrow::array::RecordBatch| {
                        for row in rows_of(&batch)? {
                            let probe = match only {
                                Some(p) => p,
                                None => match at.get(&element_key(strand, &row[0])?) {
                                    Some(&p) => p,
                                    None => continue,
                                },
                            };
                            let key: Vec<KeyValue> = levels
                                .iter()
                                .zip(&collators)
                                .enumerate()
                                .map(|(li, ((_, col), coll))| {
                                    level_key(
                                        col,
                                        *coll,
                                        order.levels[li],
                                        &row[li + 1],
                                        null_key(order, li),
                                    )
                                })
                                .collect();
                            let next = keys.len() as u32;
                            let id = match keys.get(&key) {
                                Some(&id) => id,
                                None => {
                                    let bytes =
                                        key.iter().map(|k| k.to_string().len() as u64 + 32).sum();
                                    if !held.fits(bytes) {
                                        return Err(Error::new(KEYS_PAST_THE_ROOM));
                                    }
                                    keys.insert(key, next);
                                    next
                                }
                            };
                            sets[probe].push(id);
                        }
                        Ok(())
                    },
                };
                let pages = self.reader.pages_read();
                match self.scan_one(&q, None, kept.as_ref(), &mut blocks) {
                    Ok((r, _, _)) => {
                        round.add_table(&r, &strand.orders[0].name);
                        round.reads.push(r.clone());
                        round.rows_walked += r.rows_walked;
                        round.rows_taken += r.rows_taken;
                    }
                    Err(e) if e.0 == KEYS_PAST_THE_ROOM => {
                        self.reader.read_done();
                        let read = self.reader.pages_read() - pages;
                        let r = Round {
                            order: strand.orders[0].name.clone(),
                            column_pages_predicted: read,
                            column_pages_read: read,
                            column_pages_high: read,
                            ..Default::default()
                        };
                        round.add_table(&r, &r.order);
                        round.reads.push(r);
                        past = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            if past {
                break;
            }
        }
        if past {
            return Ok((None, round, crate::room::holding(0)));
        }
        fn kept(ask: &Ask, sets: &[Vec<u32>]) -> HashSet<u32> {
            match ask {
                Ask::One(p) => sets[*p].iter().copied().collect(),
                Ask::Any(v) => v.iter().flat_map(|a| kept(a, sets)).collect(),
                Ask::All(v) => {
                    let mut it = v.iter().map(|a| kept(a, sets));
                    let first = it.next().unwrap_or_default();
                    it.fold(first, |acc, s| acc.intersection(&s).copied().collect())
                }
            }
        }
        let wanted = kept(&c.ask, &sets);
        let found: Vec<Vec<KeyValue>> = keys
            .into_iter()
            .filter(|(_, id)| wanted.contains(id))
            .map(|(k, _)| k)
            .collect();
        Ok((Some(KeySet::new(columns, found)), round, held))
    }

    /// A round priced along the strands: its pages at the most the keys can read of the table.
    pub(super) fn price_along(&self, want: &Want, c: Chosen, before: u64) -> Result<Price> {
        let part = match c.table {
            Some(p) => p,
            None => self.part(want, None)?,
        };
        let (_, most_pages, _) = part.plan.block_bounds();
        let high = part.rows.1.min(c.asking.rows.1);
        let rows = (c.asking.rows.0.min(high), high);
        let most = rows
            .1
            .saturating_mul(u64::from(most_pages))
            .min(part.plan.column_pages);
        Ok(Price {
            row_bytes: super::row_bytes(&part.plan, want.table, want.columns.iter().copied()),
            order: format!(
                "{}+{}",
                c.index, want.table.orders[part.plan.walk.order].name
            ),
            served: part.served,
            consulted: part.consulted,
            table_rows: part.table_rows,
            rows_low: rows.0,
            rows_high: rows.1,
            rows_estimate: (c.asking.estimate.round() as u64).clamp(rows.0, rows.1),
            keyless: Vec::new(),
            directory_pages: self.touched() - before,
            map_pages: part.plan.map_pages,
            column_pages: c.asking.pages + most,
            blocks: part.plan.blocks_read().min(rows.1),
        })
    }
}
