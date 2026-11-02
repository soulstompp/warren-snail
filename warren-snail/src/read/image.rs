//! The record image operators: `*=` and `*<>`, each record's fields compared by their own bytes, as
//! `record_image_eq` and `record_image_ne` compare them; and `*<`, `*<=`, `*>` and `*>=`, refused by
//! name. sqlparser reads each of them as `*` and a comparison, so a statement's tokens are joined here
//! before it parses them.

use super::pipeline::{kind_name, quoted, row_parts, Value};
use super::statement::Datum;
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::methods::bare_name;
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, TokenizerError};

/// What follows `*` to make one of these operators: its name.
fn joined(next: &Token) -> Option<&'static str> {
    Some(match next {
        Token::Eq => "*=",
        Token::Neq => "*<>",
        Token::Lt => "*<",
        Token::LtEq => "*<=",
        Token::Gt => "*>",
        Token::GtEq => "*>=",
        _ => return None,
    })
}

/// A statement's tokens, `*` and the comparison written against it one operator, as PostgreSQL's
/// lexer reads them. sqlparser reads `!=` as `<>`, and `*!=` is no operator: it stays two tokens.
pub(crate) fn tokens(sql: &str) -> std::result::Result<Vec<TokenWithSpan>, TokenizerError> {
    let tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location()?;
    if !names_an_operator(sql) {
        return Ok(tokens);
    }
    let unequal = !sql.contains("*!=");
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    for t in tokens {
        let name = match (out.last().map(|l| &l.token), joined(&t.token)) {
            (Some(Token::Mul), Some(n)) if unequal || t.token != Token::Neq => Some(n),
            _ => None,
        };
        match name {
            Some(n) => {
                let start = out.pop().expect("the star just seen").span.start;
                out.push(TokenWithSpan::new(
                    Token::CustomBinaryOperator(n.into()),
                    sqlparser::tokenizer::Span::new(start, t.span.end),
                ));
            }
            None => out.push(t),
        }
    }
    Ok(out)
}

/// Whether a statement's text may hold one of these operators.
pub(super) fn names_an_operator(sql: &str) -> bool {
    ["*=", "*<", "*>", "*!="].iter().any(|s| sql.contains(s))
}

/// The statements of `sql`, as sqlparser's PostgreSQL dialect reads them, these operators among them.
pub(crate) fn parse_sql(sql: &str) -> std::result::Result<Vec<Statement>, ParserError> {
    Parser::new(&PostgreSqlDialect {})
        .with_tokens_with_locations(tokens(sql)?)
        .parse_statements()
}

/// An expression's text as `parse_sql` reads it.
pub(super) fn parse_expr(sql: &str) -> std::result::Result<Expr, ParserError> {
    Parser::new(&PostgreSqlDialect {})
        .with_tokens_with_locations(tokens(sql)?)
        .parse_expr()
}

/// The name of one of these operators.
pub(super) fn operator(op: &BinaryOperator) -> Option<&str> {
    match op {
        BinaryOperator::Custom(s)
            if matches!(s.as_str(), "*=" | "*<>" | "*<" | "*<=" | "*>" | "*>=") =>
        {
            Some(s)
        }
        _ => None,
    }
}

/// Whether a type is a composite's, a table's row type or an anonymous record's.
fn is_record(ty: ColumnType) -> bool {
    matches!(ty, ColumnType::Composed(r) if matches!(r.def(), Composed::Record { .. }))
}

/// The refusal of those that order records.
pub(super) fn ordered(name: &str) -> Result<()> {
    match name {
        "*=" | "*<>" => Ok(()),
        _ => Err(Error::new(format!(
            "the operator {name} is not read: it orders records by PostgreSQL's in-memory layout of \
             each type, which the snail does not keep"
        ))),
    }
}

/// Whether a literal stands for a value of no type: a quoted string, or a NULL.
pub(super) fn unknown(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) => unknown(x),
        Expr::Value(v) => matches!(v.value, sqlparser::ast::Value::Null) || quoted(e),
        _ => false,
    }
}

/// What PostgreSQL's parser asks of two row constructors compared.
pub(super) fn rows_agree(left: usize, right: usize) -> Result<()> {
    if left != right {
        return Err(Error::new("unequal number of entries in row expressions"));
    }
    if left == 0 {
        return Err(Error::new("cannot compare rows of zero length"));
    }
    Ok(())
}

/// What PostgreSQL's parser asks of an operator's two operands, by their types where they are known:
/// each a record, or a literal, which it reads as a record.
pub(super) fn operands(
    name: &str,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    left: &Expr,
    right: &Expr,
) -> Result<()> {
    let other = |t: Option<ColumnType>| t.is_some_and(|t| !is_record(t));
    let shown = |t: Option<ColumnType>| t.map_or("unknown".to_string(), bare_name);
    if other(a) || other(b) {
        if (a.is_some() || unknown(left)) && (b.is_some() || unknown(right)) {
            return Err(Error::new(format!(
                "operator does not exist: {} {name} {}",
                shown(a),
                shown(b)
            )));
        }
        return Ok(());
    }
    literal_refused(left, right)
}

fn literal_refused(left: &Expr, right: &Expr) -> Result<()> {
    let literal = |e: &Expr| {
        let mut e = e;
        while let Expr::Nested(x) = e {
            e = x;
        }
        quoted(e)
    };
    if literal(left) || literal(right) {
        return Err(Error::new(
            "input of anonymous composite types is not implemented",
        ));
    }
    Ok(())
}

/// The operator over a row: two row constructors compared a pair at a time, `*=`'s pairs and'd and
/// `*<>`'s or'd, each stopping where the answer is known; anything else as one pair.
pub(super) fn answer(
    name: &str,
    left: &Expr,
    right: &Expr,
    ev: &dyn Fn(&Expr) -> Result<Value>,
) -> Result<Value> {
    ordered(name)?;
    let (Some(l), Some(r)) = (row_parts(left), row_parts(right)) else {
        return pair(name, left, right, ev);
    };
    let (l, r) = (l?, r?);
    rows_agree(l.len(), r.len())?;
    let stops = name == "*<>";
    let mut none = false;
    for (x, y) in l.iter().zip(&r) {
        match pair(name, x, y, ev)? {
            Value::Bool(b) if b == stops => return Ok(Value::Bool(b)),
            Value::Bool(_) => {}
            _ => none = true,
        }
    }
    Ok(if none { Value::Null } else { Value::Bool(!stops) })
}

/// The operator over two values: NULL over a NULL, as both functions are strict.
fn pair(
    name: &str,
    left: &Expr,
    right: &Expr,
    ev: &dyn Fn(&Expr) -> Result<Value>,
) -> Result<Value> {
    let (x, y) = (ev(left)?, ev(right)?);
    let record = |v: &Value, e: &Expr| {
        matches!(v, Value::Composed(ty, _) if is_record(*ty)) || (v.is_null() && !quoted(e))
    };
    if !record(&x, left) && !quoted(left) || !record(&y, right) && !quoted(right) {
        return Err(Error::new(format!(
            "operator does not exist: {} {name} {}",
            kind_name(&x),
            kind_name(&y)
        )));
    }
    literal_refused(left, right)?;
    if x.is_null() || y.is_null() {
        return Ok(Value::Null);
    }
    let (Value::Composed(a, p), Value::Composed(b, q)) = (&x, &y) else {
        unreachable!("two records")
    };
    let same = records_same(*a, &p.item, *b, &q.item)?;
    Ok(Value::Bool(same == (name == "*=")))
}

fn fields(ty: ColumnType) -> &'static [Field] {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            Composed::Record { fields, .. } => fields,
            _ => &[],
        },
        _ => &[],
    }
}

/// A field's type as PostgreSQL's catalog names it, without its modifier: its domain's where it has
/// one.
fn field_type(f: &Field) -> String {
    f.domain.clone().unwrap_or_else(|| bare_name(f.ty))
}

/// Two records compared as `record_image_eq` walks them: each pair of fields of one type, two NULLs
/// the same, stopping at the first that differ; two lengths refused only where no pair differs.
pub(crate) fn records_same(ta: ColumnType, a: &Item, tb: ColumnType, b: &Item) -> Result<bool> {
    let (fa, fb) = (fields(ta), fields(tb));
    let (Item::Record(va), Item::Record(vb)) = (a, b) else {
        return Ok(a == b);
    };
    for (j, ((f, x), (g, y))) in fa.iter().zip(va).zip(fb.iter().zip(vb)).enumerate() {
        let (s, t) = (field_type(f), field_type(g));
        if s != t {
            return Err(Error::new(format!(
                "cannot compare dissimilar column types {s} and {t} at record column {}",
                j + 1
            )));
        }
        if !same(f.ty, x, g.ty, y) {
            return Ok(false);
        }
    }
    if fa.len() != fb.len() {
        return Err(Error::new(
            "cannot compare record types with different numbers of columns",
        ));
    }
    Ok(true)
}

/// Whether two values of one type hold the same bytes in PostgreSQL's tuples. A record inside
/// another carries its row type, so two of different shapes differ.
fn same(ta: ColumnType, a: &Item, tb: ColumnType, b: &Item) -> bool {
    let of = |t: ColumnType| match t {
        ColumnType::Composed(r) => Some(r.def()),
        _ => None,
    };
    match (a, b) {
        (Item::Null, Item::Null) => true,
        (Item::Scalar(x), Item::Scalar(y)) => scalars_same(ta, x, tb, y),
        (
            Item::Array {
                elements: x,
                dims: dx,
                lower: lx,
            },
            Item::Array {
                elements: y,
                dims: dy,
                lower: ly,
            },
        ) => {
            let element = |t: ColumnType| match of(t) {
                Some(Composed::Array { element, .. } | Composed::Vector { element }) => *element,
                _ => t,
            };
            dx == dy
                && lx == ly
                && x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(p, q)| same(element(ta), p, element(tb), q))
        }
        (Item::Record(x), Item::Record(y)) => {
            ta == tb
                && x.len() == y.len()
                && fields(ta)
                    .iter()
                    .zip(x.iter().zip(y))
                    .all(|(f, (p, q))| same(f.ty, p, f.ty, q))
        }
        (
            Item::Range {
                flags: fx,
                lower: lx,
                upper: ux,
            },
            Item::Range {
                flags: fy,
                lower: ly,
                upper: uy,
            },
        ) => {
            let subtype = |t| crate::composed::range_of(t).map_or(t, |r| r.0);
            let bound = |p: &Option<Box<Item>>, q: &Option<Box<Item>>| match (p, q) {
                (None, None) => true,
                (Some(p), Some(q)) => same(subtype(ta), p, subtype(tb), q),
                _ => false,
            };
            fx == fy && bound(lx, ly) && bound(ux, uy)
        }
        (Item::Multirange(x), Item::Multirange(y)) => {
            let range = |t: ColumnType| match of(t) {
                Some(Composed::Multirange { range, .. }) => *range,
                _ => t,
            };
            x.len() == y.len()
                && x.iter()
                    .zip(y)
                    .all(|(p, q)| same(range(ta), p, range(tb), q))
        }
        _ => false,
    }
}

/// Two scalars: a numeric, a bit string and a type held as its text by what its output writes, which
/// their modifiers and their spellings leave one; anything else as held.
fn scalars_same(ta: ColumnType, x: &Datum, tb: ColumnType, y: &Datum) -> bool {
    match (x, y) {
        (Datum::Compound(p), Datum::Compound(q)) => same(ta, &p.item, tb, &q.item),
        _ if ta.is_numeric() || ta.is_bits() || matches!(ta, ColumnType::Textual(_)) => {
            crate::composed::scalar_out(ta, x) == crate::composed::scalar_out(tb, y)
        }
        _ => x == y,
    }
}

/// Whether two rows' values are written alike: a composed value by its parts, as its key holds its
/// class alone.
pub(super) fn held_alike(a: &[Datum], b: &[Datum]) -> bool {
    a.len() == b.len()
        && a.iter().zip(b).all(|(x, y)| match (x, y) {
            (Datum::Compound(p), Datum::Compound(q)) => p.item == q.item,
            _ => x == y,
        })
}

/// The record a row constructor makes of its values: its fields named as PostgreSQL names them, each
/// of its value's type, a literal's kept apart as `unknown`, which is no other type.
pub(super) fn anonymous(types: &[(ColumnType, bool)]) -> ColumnType {
    crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields: types
            .iter()
            .enumerate()
            .map(|(i, &(ty, literal))| {
                let mut f = Field::new(&format!("f{}", i + 1), ty);
                if literal {
                    f.domain = Some("unknown".into());
                }
                f
            })
            .collect(),
    })
}

/// A row constructor's fields, each its value's type or, for a literal, text as `unknown`; none where
/// a value's type is not known.
pub(super) fn field_types(
    parts: &[&Expr],
    types: &[Option<ColumnType>],
) -> Option<Vec<(ColumnType, bool)>> {
    parts
        .iter()
        .zip(types)
        .map(|(e, t)| match t {
            Some(t) => Some((*t, false)),
            None if unknown(e) => Some((ColumnType::Text(None), true)),
            None => None,
        })
        .collect()
}
