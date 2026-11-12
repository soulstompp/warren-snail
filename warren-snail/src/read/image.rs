//! The record image operators: `*=` and `*<>`, each record's fields compared by their own bytes, as
//! `record_image_eq` and `record_image_ne` compare them; and `*<`, `*<=`, `*>` and `*>=`, refused by
//! name. sqlparser reads each of them as `*` and a comparison, so a statement's tokens are joined here
//! before it parses them; and it reads no operator of its own before ANY, SOME or ALL, so an array
//! there is handed to `=` inside a call whose name carries the operator.

use super::pipeline::{kind_name, quoted, row_parts, Value};
use super::statement::Datum;
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::methods::bare_name;
use crate::error::NO_OPERATOR;
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Statement};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer, TokenizerError, Word};

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

/// Whether a token sqlparser reads as `<>` is written `!=`.
fn written_bang(sql: &str, t: &TokenWithSpan) -> bool {
    let at = t.span.start;
    t.token == Token::Neq
        && sql.lines().nth(at.line as usize - 1).is_some_and(|l| {
            l.chars().nth(at.column as usize - 1) == Some('!')
        })
}

/// A statement's tokens, `*` and the comparison written against it one operator, as PostgreSQL's
/// lexer reads them. sqlparser reads `!=` as `<>`, and `*!=` is no operator: it stays two tokens.
/// `:=`, which PostgreSQL's grammar reads only between an argument's name and its value, is read
/// as `=>`, the spelling sqlparser reads there.
pub(crate) fn tokens(sql: &str) -> std::result::Result<Vec<TokenWithSpan>, TokenizerError> {
    let mut tokens = Tokenizer::new(&PostgreSqlDialect {}, sql).tokenize_with_location()?;
    if sql.contains(":=") {
        for t in &mut tokens {
            if t.token == Token::Assignment {
                t.token = Token::RArrow;
            }
        }
    }
    if !names_an_operator(sql) {
        return Ok(tokens);
    }
    let mut out: Vec<TokenWithSpan> = Vec::with_capacity(tokens.len());
    for t in tokens {
        let name = match (out.last().map(|l| &l.token), joined(&t.token)) {
            (Some(Token::Mul), Some(n)) if !written_bang(sql, &t) => Some(n),
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
    Ok(over_arrays(out))
}

/// What marks a call as carrying an operator before ANY, SOME or ALL: its name is this character and
/// the operator's.
const MARK: char = '\u{1}';

/// Each of these operators before ANY, SOME or ALL over an array: `=`, the array inside the call that
/// names it. Over a subquery it stays as it is, and sqlparser refuses it, naming it.
fn over_arrays(mut t: Vec<TokenWithSpan>) -> Vec<TokenWithSpan> {
    let solid = |t: &[TokenWithSpan], from: usize| {
        (from..t.len()).find(|&i| !matches!(t[i].token, Token::Whitespace(_)))
    };
    let call = |text: String| Word {
        value: text,
        quote_style: Some('"'),
        keyword: Keyword::NoKeyword,
    };
    let mut i = 0;
    while i < t.len() {
        let name = match &t[i].token {
            Token::CustomBinaryOperator(n) if joined_name(n) => n.clone(),
            _ => {
                i += 1;
                continue;
            }
        };
        i += 1;
        let Some(word) = solid(&t, i) else { break };
        let Some(open) = solid(&t, word + 1) else { break };
        let Some(first) = solid(&t, open + 1) else { break };
        let quantified = matches!(&t[word].token, Token::Word(w)
            if w.quote_style.is_none() && matches!(w.keyword, Keyword::ANY | Keyword::SOME | Keyword::ALL));
        let over_a_query = matches!(&t[first].token, Token::Word(w)
            if w.quote_style.is_none() && matches!(w.keyword, Keyword::SELECT | Keyword::WITH | Keyword::VALUES));
        if !quantified || t[open].token != Token::LParen || over_a_query {
            continue;
        }
        let mut depth = 0;
        let Some(close) = (open..t.len()).find(|&k| {
            match t[k].token {
                Token::LParen => depth += 1,
                Token::RParen => depth -= 1,
                _ => {}
            }
            depth == 0
        }) else {
            break;
        };
        let at = |token: Token, like: &TokenWithSpan| TokenWithSpan::new(token, like.span);
        t[i - 1].token = Token::Eq;
        let closing = at(Token::RParen, &t[close]);
        t.insert(close, closing);
        let opening = at(Token::LParen, &t[open]);
        t.insert(open + 1, opening);
        let named = at(Token::Word(call(format!("{MARK}{name}"))), &t[open]);
        t.insert(open + 1, named);
    }
    t
}

/// The operator and the array a call carries before ANY, SOME or ALL.
pub(super) fn marked(right: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Function(f) = right else {
        return None;
    };
    let [part] = f.name.0.as_slice() else {
        return None;
    };
    let name = part.as_ident()?.value.strip_prefix(MARK)?;
    let FunctionArguments::List(l) = &f.args else {
        return None;
    };
    match l.args.as_slice() {
        [FunctionArg::Unnamed(FunctionArgExpr::Expr(x))] => Some((name, x)),
        _ => None,
    }
}

fn joined_name(name: &str) -> bool {
    matches!(name, "*=" | "*<>" | "*<" | "*<=" | "*>" | "*>=")
}

/// Whether a statement's text may hold one of these operators.
fn names_an_operator(sql: &str) -> bool {
    ["*=", "*<", "*>", "\u{1}*"].iter().any(|s| sql.contains(s))
}

/// Whether a statement holds one of these operators, outside a string.
pub(super) fn asks_an_operator(sql: &str) -> bool {
    names_an_operator(sql)
        && tokens(sql).is_ok_and(|t| {
            t.iter().any(|t| match &t.token {
                Token::CustomBinaryOperator(n) => joined_name(n),
                Token::Word(w) => w.quote_style.is_some() && w.value.starts_with(MARK),
                _ => false,
            })
        })
}

/// The statements of `sql`, as sqlparser's PostgreSQL dialect reads them, these operators among them,
/// and what it takes past PostgreSQL's grammar refused by name (`unread`).
pub(crate) fn parse_sql(sql: &str) -> Result<Vec<Statement>> {
    let mut statements = Parser::new(&PostgreSqlDialect {})
        .with_tokens_with_locations(tokens(sql).map_err(ParserError::from)?)
        .parse_statements()?;
    super::unread::checked(sql, &mut statements)?;
    Ok(statements)
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
        BinaryOperator::Custom(s) if joined_name(s) => Some(s),
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
                "operator does not exist: {} {name} {}{NO_OPERATOR}",
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
            "operator does not exist: {} {name} {}{NO_OPERATOR}",
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

/// record_image_eq and record_image_ne: two records compared by their images, a NULL field the same
/// as a NULL field; NULL over a NULL record, as both functions are strict.
pub(super) fn records_compared(eq: bool, x: Value, y: Value) -> Result<Value> {
    if x.is_null() || y.is_null() {
        return Ok(Value::Null);
    }
    match (&x, &y) {
        (Value::Composed(a, p), Value::Composed(b, q)) if is_record(*a) && is_record(*b) => {
            Ok(Value::Bool(records_same(*a, &p.item, *b, &q.item)? == eq))
        }
        _ => Err(Error::new(format!(
            "function record_image_{}({}, {}) does not exist",
            if eq { "eq" } else { "ne" },
            kind_name(&x),
            kind_name(&y)
        ))),
    }
}

/// What PostgreSQL's parser asks of an operator before ANY, SOME or ALL: an array on its right, whose
/// element it asks as a right operand.
pub(super) fn over_array_operands(
    name: &str,
    a: Option<ColumnType>,
    b: Option<ColumnType>,
    left: &Expr,
    array: &Expr,
) -> Result<()> {
    let element = match b {
        Some(t) => match super::typed::element_of(t) {
            Some(e) => Some(e),
            None => return Err(not_an_array()),
        },
        None => None,
    };
    let other = |t: Option<ColumnType>| t.is_some_and(|t| !is_record(t));
    if element.is_some() && (other(a) || other(element)) {
        return operands(name, a, element, left, array);
    }
    if other(a) && unknown(array) {
        return Err(not_an_array());
    }
    Ok(())
}

fn not_an_array() -> Error {
    Error::new("op ANY/ALL (array) requires array on right side")
}

/// The operator between a value and each element of an array, ANY's or ALL's: true where one, or
/// every one, answers it, NULL where none does and one is NULL, as PostgreSQL's strict operators over
/// an array are; over no element false, or true.
pub(super) fn over_array(
    name: &str,
    x: Value,
    array: Value,
    left: &Expr,
    right: &Expr,
    any: bool,
) -> Result<Value> {
    ordered(name)?;
    if quoted(left) {
        literal_refused(left, right)?;
    }
    let (ty, c) = match array {
        Value::Null => return Ok(Value::Null),
        Value::Composed(ty, c) if super::typed::element_of(ty).is_some() => (ty, c),
        Value::Text(s) if quoted(right) && s.trim() == "{}" => return Ok(Value::Bool(!any)),
        Value::Text(_) if quoted(right) => {
            return Err(Error::new(
                "input of anonymous composite types is not implemented",
            ))
        }
        _ => return Err(not_an_array()),
    };
    let element = super::typed::element_of(ty).expect("an array's element");
    let Item::Array { elements, .. } = &c.item else {
        return Ok(Value::Null);
    };
    if elements.is_empty() {
        return Ok(Value::Bool(!any));
    }
    if x.is_null() {
        return Ok(Value::Null);
    }
    let (Value::Composed(a, p), true) = (&x, is_record(element)) else {
        return Err(Error::new(format!(
            "operator does not exist: {} {name} {}{NO_OPERATOR}",
            kind_name(&x),
            element.pg_name()
        )));
    };
    let mut none = false;
    for e in elements {
        if e.is_null() {
            none = true;
            continue;
        }
        let answer = records_same(*a, &p.item, element, e)? == (name == "*=");
        if answer == any {
            return Ok(Value::Bool(any));
        }
    }
    Ok(if none { Value::Null } else { Value::Bool(!any) })
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
    anonymous_of(types, &[])
}

/// Whether a type is a row constructor's record whose fields are of `fields`' types.
pub(super) fn is_anonymous(ty: ColumnType, fields: &[(ColumnType, bool)]) -> bool {
    matches!(ty, ColumnType::Composed(r)
        if matches!(r.def(), Composed::Record { name, .. } if name == "record"))
        && is_record_of(ty, fields)
}

/// Whether a type is a record or a row type whose fields are of `fields`' types: a relation's whole
/// row's.
pub(super) fn is_record_of(ty: ColumnType, fields: &[(ColumnType, bool)]) -> bool {
    matches!(ty, ColumnType::Composed(r)
        if matches!(r.def(), Composed::Record { fields: f, .. }
            if f.len() == fields.len()
                && f.iter().zip(fields).all(|(a, b)| a.ty == b.0)))
}

/// The record a row constructor makes, each field of the domain its value's column is of, where
/// `domains` names one.
pub(super) fn anonymous_of(types: &[(ColumnType, bool)], domains: &[Option<String>]) -> ColumnType {
    crate::composed::type_of(Composed::Record {
        name: "record".into(),
        fields: types
            .iter()
            .enumerate()
            .map(|(i, &(ty, literal))| {
                let mut f = Field::new(&format!("f{}", i + 1), ty);
                if literal {
                    f.domain = Some("unknown".into());
                } else if let Some(Some(d)) = domains.get(i) {
                    f.domain = Some(d.clone());
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
