//! Prepared statements, as PostgreSQL's PREPARE keeps them for its session: each its name, the types
//! of its parameters and its statement, run by EXECUTE with its parameters' values. A parameter's
//! type is the one PREPARE names, else the one PostgreSQL's parse analysis settles where it stands
//! (`deparse::params`), and PREPARE is refused where that reading refuses it; where the reading
//! reads no such statement, the type the column it meets gives it, or text. pg_prepared_statement()
//! gives the session's prepared statements as PostgreSQL's does, but for a plan cache's counts.

use super::{Snail, Value};
use crate::catalog::{Catalog, ColumnType};
use crate::regs::RegKind;
use crate::{Error, Result};
use sqlparser::ast::{Expr, SetExpr, Statement as Sql};
use std::cell::RefCell;
use std::rc::Rc;

/// A statement PREPARE keeps.
#[derive(Clone, Debug)]
pub(super) struct Prepared {
    /// Each parameter's type by its name, as PREPARE names it or as where it stands settles it;
    /// none where neither does, and it is read as a literal is, by its context.
    pub(super) types: Vec<Option<String>>,
    /// Each parameter's type by its OID, where PostgreSQL's reading of the statement settled
    /// them all.
    pub(super) oids: Option<Vec<u32>>,
    /// Where that reading settled them, each column's type by its OID of the rows the statement
    /// returns; none where it returns none, a write without RETURNING.
    pub(super) results: Option<Vec<u32>>,
    /// Each reference to a parameter, in the order they stand: how its value is written there.
    pub(super) written: Vec<Written>,
    /// The statement, as written.
    pub(super) statement: String,
    /// The text the PREPARE came in, whole, as the session was handed it.
    pub(super) source: String,
    /// When the PREPARE ran: its statement's clock.
    pub(super) at: i64,
}

/// How a parameter's value is written where a reference to it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Written {
    /// As a literal, which its context reads as the parameter's type.
    Bare,
    /// Cast to the parameter's type, which a literal standing there would be read as no longer.
    Cast,
    /// As a literal where its value reads the same as this integer type, which a literal standing
    /// there is read as, the parameter's being another integer type; else cast.
    BareAs(String),
}

/// smallint, integer and bigint.
fn integer(oid: u32) -> bool {
    matches!(oid, 20 | 21 | 23)
}

/// The type a value of `ty` is worked out as: a file's domain's base type, whose checks the
/// statement makes where the value stands.
fn worked_out_as<'a>(catalog: &'a Catalog, ty: &'a str) -> &'a str {
    let mut t = ty;
    for _ in 0..catalog.domains.len() {
        let found = catalog.domains.iter().find(|d| {
            let full = crate::oids::full(&d.name);
            d.name == t || full == t || full == format!("public.{t}")
        });
        match found {
            Some(d) => t = &d.base,
            None => break,
        }
    }
    t
}

/// The highest parameter, `$n`, a statement names.
fn highest(sql: &str) -> Result<usize> {
    Ok(super::statement::spans(sql)?
        .into_iter()
        .filter_map(|(t, _, _)| match t {
            sqlparser::tokenizer::Token::Placeholder(p) => {
                p.strip_prefix('$').and_then(|n| n.parse::<usize>().ok())
            }
            _ => None,
        })
        .max()
        .unwrap_or(0))
}

/// The number of a parameter an expression is, `$n`, with its casts taken off.
fn param_of(e: &Expr) -> Option<usize> {
    match e {
        Expr::Value(v) => match &v.value {
            sqlparser::ast::Value::Placeholder(p) => p.strip_prefix('$')?.parse().ok(),
            _ => None,
        },
        Expr::Nested(x) => param_of(x),
        _ => None,
    }
}

/// The types a statement's parameters take from the columns they meet: compared with a column, in a
/// column's place of an INSERT or set to a column by an UPDATE; and LIMIT and OFFSET a bigint's.
fn inferred(catalog: &Catalog, sql: &str, n: usize) -> Vec<Option<String>> {
    let mut out = vec![None; n];
    let Ok(parsed) = super::image::parse_sql(sql) else {
        return out;
    };
    let Some(st) = parsed.first() else {
        return out;
    };
    // The tables it names: each name after FROM, JOIN, INTO or UPDATE that the catalog holds.
    let tables: Vec<&crate::catalog::Table> = {
        use sqlparser::tokenizer::Token;
        let spans = super::statement::spans(sql).unwrap_or_default();
        let w: Vec<&Token> = spans
            .iter()
            .map(|(t, _, _)| t)
            .filter(|t| !matches!(t, Token::Whitespace(_)))
            .collect();
        let mut out = Vec::new();
        for i in 0..w.len() {
            let after = matches!(w[i], Token::Word(x) if x.quote_style.is_none()
                && matches!(x.value.to_uppercase().as_str(), "FROM" | "JOIN" | "INTO" | "UPDATE"));
            if !after {
                continue;
            }
            let mut name = Vec::new();
            let mut j = i + 1;
            while let Some(Token::Word(x)) = w.get(j) {
                name.push(match x.quote_style {
                    Some(_) => x.value.clone(),
                    None => x.value.to_lowercase(),
                });
                if matches!(w.get(j + 1), Some(Token::Period)) {
                    j += 2;
                } else {
                    break;
                }
            }
            if let Some(t) = catalog.table(&name.join(".")) {
                out.push(t);
            }
        }
        out
    };
    let column = |e: &Expr| -> Option<String> {
        let last = match e {
            Expr::Identifier(i) => crate::catalog::ident(i),
            Expr::CompoundIdentifier(parts) => crate::catalog::ident(parts.last()?),
            _ => return None,
        };
        let mut found = tables
            .iter()
            .flat_map(|t| t.columns.iter())
            .filter(|c| c.name == last);
        let c = found.next()?;
        found.next().is_none().then(|| c.ty.pg_name())
    };
    let mut set = |k: usize, ty: Option<String>| {
        if let (Some(slot), Some(ty)) = (out.get_mut(k.wrapping_sub(1)), ty) {
            slot.get_or_insert(ty);
        }
    };
    // Each condition of its WHERE, walked through AND, OR, NOT and parentheses.
    fn walk(e: &Expr, each: &mut dyn FnMut(&Expr)) {
        each(e);
        match e {
            Expr::BinaryOp { left, right, .. } => {
                walk(left, each);
                walk(right, each);
            }
            Expr::UnaryOp { expr, .. } | Expr::Nested(expr) => walk(expr, each),
            _ => {}
        }
    }
    let wheres: Vec<&Expr> = match st {
        Sql::Query(q) => match q.body.as_ref() {
            SetExpr::Select(s) => s.selection.iter().chain(s.having.iter()).collect(),
            _ => Vec::new(),
        },
        Sql::Update(u) => u.selection.iter().collect(),
        Sql::Delete(d) => d.selection.iter().collect(),
        _ => Vec::new(),
    };
    for w in wheres {
        walk(w, &mut |e: &Expr| match e {
            Expr::BinaryOp { left, right, .. } => {
                if let Some(k) = param_of(right) {
                    set(k, column(left));
                }
                if let Some(k) = param_of(left) {
                    set(k, column(right));
                }
            }
            Expr::InList { expr, list, .. } => {
                for x in list {
                    if let Some(k) = param_of(x) {
                        set(k, column(expr));
                    }
                }
            }
            Expr::Between {
                expr, low, high, ..
            } => {
                for x in [low, high] {
                    if let Some(k) = param_of(x) {
                        set(k, column(expr));
                    }
                }
            }
            _ => {}
        });
    }
    match st {
        Sql::Insert(i) => {
            let name = match &i.table {
                sqlparser::ast::TableObject::TableName(n) => crate::catalog::object_name(n),
                _ => String::new(),
            };
            if let (Some(t), Some(q)) = (catalog.table(&name), i.source.as_deref()) {
                let cols: Vec<String> = if i.columns.is_empty() {
                    t.columns.iter().map(|c| c.ty.pg_name()).collect()
                } else {
                    i.columns
                        .iter()
                        .filter_map(|c| {
                            let n = crate::catalog::object_name(c);
                            t.columns.iter().find(|x| x.name == n).map(|x| x.ty.pg_name())
                        })
                        .collect()
                };
                let rows: Vec<Vec<&Expr>> = match q.body.as_ref() {
                    SetExpr::Values(v) => v.rows.iter().map(|r| r.content.iter().collect()).collect(),
                    SetExpr::Select(s) => vec![s
                        .projection
                        .iter()
                        .filter_map(|p| match p {
                            sqlparser::ast::SelectItem::UnnamedExpr(e)
                            | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
                            _ => None,
                        })
                        .collect()],
                    _ => Vec::new(),
                };
                for r in rows {
                    for (e, ty) in r.iter().zip(&cols) {
                        if let Some(k) = param_of(e) {
                            set(k, Some(ty.clone()));
                        }
                    }
                }
            }
        }
        Sql::Update(u) => {
            for a in &u.assignments {
                if let (sqlparser::ast::AssignmentTarget::ColumnName(c), Some(k)) =
                    (&a.target, param_of(&a.value))
                {
                    let n = crate::catalog::object_name(c);
                    let ty = tables
                        .iter()
                        .flat_map(|t| t.columns.iter())
                        .find(|x| x.name == n)
                        .map(|x| x.ty.pg_name());
                    set(k, ty);
                }
            }
        }
        Sql::Query(q) => {
            if let Some(l) = &q.limit_clause {
                let s = l.to_string();
                for (t, _, _) in super::statement::spans(&s).unwrap_or_default() {
                    if let sqlparser::tokenizer::Token::Placeholder(p) = t {
                        if let Some(k) = p.strip_prefix('$').and_then(|n| n.parse().ok()) {
                            set(k, Some("bigint".into()));
                        }
                    }
                }
            }
            // A parameter a SELECT returns as a column with nothing else to type it is text.
            if let SetExpr::Select(s) = q.body.as_ref() {
                for p in &s.projection {
                    if let sqlparser::ast::SelectItem::UnnamedExpr(e)
                    | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } = p
                    {
                        if let Some(k) = param_of(e) {
                            set(k, Some("text".into()));
                        }
                    }
                }
            }
        }
        _ => {}
    }
    out
}

/// PREPARE name [(type, …)] AS statement, read off its words: none where it is no such statement.
pub(super) fn parse_prepare(sql: &str) -> Option<Result<(String, Vec<Option<String>>, String)>> {
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
    if kw(0) != "PREPARE" || kw(1) == "TRANSACTION" {
        return None;
    }
    let syntax = |i: usize| {
        Some(Err(Error::new(match words.get(i) {
            Some((t, _, _)) => format!("syntax error at or near \"{t}\""),
            None => "syntax error at end of input".into(),
        })))
    };
    let name = match words.get(1).map(|w| &w.0) {
        Some(Token::Word(w)) => crate::catalog::ident(&sqlparser::ast::Ident {
            value: w.value.clone(),
            quote_style: w.quote_style,
            span: sqlparser::tokenizer::Span::empty(),
        }),
        _ => return syntax(1),
    };
    let mut i = 2;
    let mut types = Vec::new();
    if matches!(words.get(i).map(|w| &w.0), Some(Token::LParen)) {
        let mut depth = 0;
        let mut from = words[i].2;
        i += 1;
        loop {
            match words.get(i).map(|w| &w.0) {
                None => return syntax(i),
                Some(Token::LParen) => depth += 1,
                Some(Token::RParen) if depth > 0 => depth -= 1,
                Some(Token::Comma | Token::RParen) if depth == 0 => {
                    let ty = sql[from..words[i].1].trim().to_string();
                    if ty.is_empty() {
                        return syntax(i);
                    }
                    types.push(match ty.eq_ignore_ascii_case("unknown") {
                        true => None,
                        false => Some(ty),
                    });
                    if matches!(words[i].0, Token::RParen) {
                        i += 1;
                        break;
                    }
                    from = words[i].2;
                }
                _ => {}
            }
            i += 1;
        }
    }
    if kw(i) != "AS" {
        return syntax(i);
    }
    let Some(start) = words.get(i + 1).map(|w| w.1) else {
        return syntax(i + 1);
    };
    let body = sql[start..].trim().trim_end_matches(';').trim_end().to_string();
    Some(Ok((name, types, body)))
}

/// A session's prepared statements, by name.
pub(super) type Kept = std::collections::BTreeMap<String, Prepared>;

thread_local! {
    /// The session's prepared statements while one of its statements runs, as
    /// pg_prepared_statement() reads them.
    static VIEWED: RefCell<Option<Rc<Kept>>> = const { RefCell::new(None) };
}

/// The session's prepared statements, held while a statement runs; those held before come back
/// where it is dropped.
pub(super) struct Viewing(Option<Rc<Kept>>);

pub(super) fn viewing(kept: &Rc<Kept>) -> Viewing {
    Viewing(VIEWED.with(|v| v.replace(Some(kept.clone()))))
}

impl Drop for Viewing {
    fn drop(&mut self) {
        let before = self.0.take();
        VIEWED.with(|v| *v.borrow_mut() = before);
    }
}

/// The columns of pg_prepared_statement()'s rows under a release, each its name and type:
/// result_types from 16.
fn columns(release: u8) -> Vec<(&'static str, ColumnType)> {
    let regtypes = super::typed::array_of(ColumnType::Reg(RegKind::Type, release));
    let mut out = vec![
        ("name", ColumnType::Text(None)),
        ("statement", ColumnType::Text(None)),
        ("prepare_time", ColumnType::Timestamptz),
        ("parameter_types", regtypes),
    ];
    if release >= 16 {
        out.push(("result_types", regtypes));
    }
    out.extend([
        ("from_sql", ColumnType::Boolean),
        ("generic_plans", ColumnType::Int8),
        ("custom_plans", ColumnType::Int8),
    ]);
    out
}

/// The columns of pg_prepared_statement()'s rows only a plan cache fills: how many times each
/// statement was run on its generic plan and on a custom one.
pub(super) const PLAN_CACHE: [&str; 2] = ["generic_plans", "custom_plans"];

/// Why a column of a plan cache's is not read.
pub(super) fn plan_cache_refusal(column: &str) -> String {
    format!("{column} is a plan cache's count")
}

/// The type of pg_prepared_statement()'s rows under a release; none for any other name.
pub(super) fn typed(name: &str, release: u8) -> Option<ColumnType> {
    (name == "pg_prepared_statement").then(|| {
        crate::composed::type_of(crate::composed::Composed::Record {
            name: "record".into(),
            fields: columns(release)
                .into_iter()
                .map(|(n, t)| crate::composed::Field::new(n, t))
                .collect(),
        })
    })
}

/// pg_prepared_statement()'s rows: the session's prepared statements, each a record of `ty`, its
/// columns as PostgreSQL's fills them but a plan cache's, which are NULL and refused by name where
/// a statement reads them. Refused where a statement's types were not settled as PostgreSQL's
/// reading settles them. None for any other name.
pub(super) fn rows(name: &str, given: usize, ty: Option<ColumnType>) -> Result<Option<Vec<Value>>> {
    if name != "pg_prepared_statement" {
        return Ok(None);
    }
    if given > 0 {
        return Err(Error::new(format!(
            "function pg_prepared_statement({}) does not exist",
            vec!["unknown"; given].join(", ")
        )));
    }
    let Some(ColumnType::Composed(r)) = ty else {
        return Err(Error::new("pg_prepared_statement() is read in FROM"));
    };
    let crate::composed::Composed::Record { fields, .. } = r.def() else {
        return Err(Error::new("pg_prepared_statement() is read in FROM"));
    };
    let kept = VIEWED.with(|v| v.borrow().clone()).unwrap_or_default();
    let mut out = Vec::with_capacity(kept.len());
    for (n, p) in kept.iter() {
        let Some(oids) = &p.oids else {
            return Err(Error::new(format!(
                "the types of prepared statement \"{n}\" were not settled as PostgreSQL's parse \
                 analysis settles them: pg_prepared_statement() is not read"
            )));
        };
        let mut items = Vec::with_capacity(fields.len());
        for f in fields {
            let v = match f.name.as_str() {
                "name" => Value::Text(n.clone()),
                "statement" => Value::Text(p.source.clone()),
                "prepare_time" => Value::Scalar(ColumnType::Timestamptz, p.at),
                "parameter_types" => regtypes(f.ty, oids)?,
                "result_types" => match &p.results {
                    Some(r) => regtypes(f.ty, r)?,
                    None => Value::Null,
                },
                "from_sql" => Value::Bool(true),
                _ => Value::Null,
            };
            items.push(super::pipeline::part_of(f.ty, v, false)?);
        }
        out.push(super::pipeline::composed(ty.unwrap(), crate::composed::Item::Record(items)));
    }
    Ok(Some(out))
}

/// A regtype[] of types by their OIDs.
fn regtypes(ty: ColumnType, oids: &[u32]) -> Result<Value> {
    let element = super::typed::element_of(ty).expect("an array of regtype");
    let elements = oids
        .iter()
        .map(|&o| {
            super::pipeline::part_of(
                element,
                Value::Scalar(element, crate::scalars::unsigned_key(u64::from(o), 32)),
                false,
            )
        })
        .collect::<Result<Vec<_>>>()?;
    let (dims, lower) = match elements.len() {
        0 => (Vec::new(), Vec::new()),
        n => (vec![n as i32], vec![1]),
    };
    Ok(super::pipeline::composed(
        ty,
        crate::composed::Item::Array {
            elements,
            dims,
            lower,
        },
    ))
}

/// PREPARE: the statement kept under `name`, each parameter's type named or settled where it
/// stands; `source` the text it came in, `at` its statement's clock.
pub(super) fn prepare(
    kept: &mut Kept,
    catalog: &Catalog,
    name: &str,
    given: Vec<Option<String>>,
    body: &str,
    (source, at): (&str, i64),
) -> Result<()> {
    if kept.contains_key(name) {
        return Err(Error::new(format!(
            "prepared statement \"{name}\" already exists"
        )));
    }
    let first = body
        .split(|c: char| !c.is_ascii_alphabetic())
        .find(|w| !w.is_empty())
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(
        first.as_str(),
        "SELECT" | "INSERT" | "UPDATE" | "DELETE" | "VALUES" | "WITH" | "TABLE" | "MERGE"
    ) {
        return Err(Error::new(format!("syntax error at or near \"{first}\"")));
    }
    use crate::deparse::params;
    // typenameTypeId: each type named, read before the statement, its typmod dropped.
    let named: Vec<u32> = given
        .iter()
        .map(|g| g.as_deref().map_or(Ok(0), |t| params::named_type(catalog, t)))
        .collect::<Result<_>>()?;
    let (text, refs) = params::each_reference(body)?;
    let prepared = match params::reading(catalog, body, &named, true) {
        Ok(params::Reading {
            params: oids,
            results,
        }) => {
            let names = params::type_names(catalog, &oids);
            // Each reference's own type, as a literal standing there would be read.
            let own = params::types(catalog, &text, &[], false).ok();
            let own_names = own.as_deref().map(|o| params::type_names(catalog, o));
            let written = refs
                .iter()
                .enumerate()
                .map(|(i, &k)| {
                    let slot = oids[k - 1];
                    match (&own, &own_names) {
                        (Some(own), Some(n)) => match own.get(i).copied() {
                            Some(t) if t == slot => Written::Bare,
                            Some(t) if integer(t) && integer(slot) => Written::BareAs(n[i].clone()),
                            _ => Written::Cast,
                        },
                        _ if named.get(k - 1).is_some_and(|&t| t != 0) => Written::Cast,
                        _ => Written::Bare,
                    }
                })
                .collect();
            Prepared {
                types: names.into_iter().map(Some).collect(),
                oids: Some(oids),
                results,
                written,
                statement: body.to_string(),
                source: source.to_string(),
                at,
            }
        }
        Err(e) if params::refuses(&e) => return Err(e),
        // A statement the reading does not read: each parameter typed by the column it meets.
        Err(_) => {
            let n = highest(body)?.max(given.len());
            let inferred = inferred(catalog, body, n);
            let named_names = params::type_names(catalog, &named);
            let types: Vec<Option<String>> = (0..n)
                .map(|k| match named.get(k) {
                    Some(&t) if t != 0 => Some(named_names[k].clone()),
                    _ => inferred[k].clone(),
                })
                .collect();
            let returned = returned_params(body);
            let written = refs
                .iter()
                .map(|k| match types[k - 1].is_some() && returned.contains(k) {
                    true => Written::Cast,
                    false => Written::Bare,
                })
                .collect();
            Prepared {
                types,
                oids: None,
                results: None,
                written,
                statement: body.to_string(),
                source: source.to_string(),
                at,
            }
        }
    };
    kept.insert(name.to_string(), prepared);
    Ok(())
}

/// EXECUTE: the statement kept under `name`, each parameter's value worked out from its expression
/// and cast to its type, written where it stands: the statement to run.
pub(super) fn bound(kept: &Kept, catalog: &Catalog, name: &str, args: &[String]) -> Result<String> {
    {
        let p = kept.get(name).cloned().ok_or_else(|| {
            Error::new(format!("prepared statement \"{name}\" does not exist"))
        })?;
        if args.len() != p.types.len() {
            return Err(Error::new(format!(
                "wrong number of parameters for prepared statement \"{name}\": Expected {} \
                 parameters but got {}.",
                p.types.len(),
                args.len()
            )));
        }
        let casings = super::pipeline::Casings::of_file(catalog.monetary, catalog.text_search);
        // A value worked out as text.
        let evaluated = |a: &str, text: &str| -> Result<Option<String>> {
            let parsed = super::image::parse_sql(&format!("SELECT {text}"))?;
            let e = match parsed.as_slice() {
                [Sql::Query(q)] => match q.body.as_ref() {
                    SetExpr::Select(s) => match s.projection.first() {
                        Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) => e.clone(),
                        _ => return Err(Error::new(format!("syntax error at or near \"{a}\""))),
                    },
                    _ => return Err(Error::new(format!("syntax error at or near \"{a}\""))),
                },
                _ => return Err(Error::new(format!("syntax error at or near \"{a}\""))),
            };
            Ok(super::pipeline::eval_on(&e, &[], &[], catalog.release, &casings)?.text())
        };
        let mut values = Vec::with_capacity(args.len());
        for (k, (a, ty)) in args.iter().zip(&p.types).enumerate() {
            let expected = p.oids.as_ref().map_or(0, |o| o[k]);
            crate::deparse::params::argument(catalog, k + 1, a, expected)?;
            let text = match ty {
                Some(ty) => format!("CAST(CAST(({a}) AS {}) AS text)", worked_out_as(catalog, ty)),
                None => format!("CAST(({a}) AS text)"),
            };
            values.push(evaluated(a, &text)?);
        }
        // Each parameter where it stands: a literal its context reads as the parameter's type, or
        // its value cast to that type where a literal there would be read as another.
        let mut out = String::with_capacity(p.statement.len());
        let mut from = 0;
        let mut at = 0;
        for (t, a, b) in super::statement::spans(&p.statement)? {
            let sqlparser::tokenizer::Token::Placeholder(ph) = t else {
                continue;
            };
            let Some(k) = ph.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            let cast = match (p.written.get(at), &values[k - 1]) {
                (Some(Written::Cast), _) => true,
                (Some(Written::BareAs(t)), Some(v)) => {
                    let quoted = v.replace('\'', "''");
                    evaluated(v, &format!("CAST(CAST('{quoted}' AS {t}) AS text)")).ok().flatten().as_ref() != Some(v)
                }
                _ => false,
            };
            at += 1;
            out.push_str(&p.statement[from..a]);
            match (&values[k - 1], &p.types[k - 1]) {
                (None, Some(ty)) if cast => out.push_str(&format!("CAST(NULL AS {ty})")),
                (None, _) => out.push_str("NULL"),
                (Some(v), Some(ty)) if cast => {
                    out.push_str(&format!("CAST('{}' AS {ty})", v.replace('\'', "''")))
                }
                (Some(v), _) => out.push_str(&format!("'{}'", v.replace('\'', "''"))),
            }
            from = b;
        }
        out.push_str(&p.statement[from..]);
        Ok(out)
    }
}

/// DEALLOCATE: the statement kept under `name` let go, or every one.
pub(super) fn deallocate(kept: &mut Kept, name: Option<&str>) -> Result<()> {
    {
        match name {
            None => kept.clear(),
            Some(n) => {
                if kept.remove(n).is_none() {
                    return Err(Error::new(format!(
                        "prepared statement \"{n}\" does not exist"
                    )));
                }
            }
        }
        Ok(())
    }
}

impl Snail {
    pub(super) fn execute_prepared(&mut self, name: &str, args: &[String]) -> Result<super::Done> {
        let text = bound(&self.session.prepared, &self.catalog, name, args)?;
        self.execute(&text)
    }
}

/// The parameters a SELECT returns whole as its columns.
fn returned_params(sql: &str) -> Vec<usize> {
    let Ok(parsed) = super::image::parse_sql(sql) else {
        return Vec::new();
    };
    let Some(Sql::Query(q)) = parsed.first() else {
        return Vec::new();
    };
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Vec::new();
    };
    s.projection
        .iter()
        .filter_map(|p| match p {
            sqlparser::ast::SelectItem::UnnamedExpr(e)
            | sqlparser::ast::SelectItem::ExprWithAlias { expr: e, .. } => param_of(e),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_prepared_statements_rows_carry_result_types_from_16() {
        let names = |release: u8| -> Vec<String> {
            let Some(ColumnType::Composed(r)) = typed("pg_prepared_statement", release) else {
                panic!("a record")
            };
            let crate::composed::Composed::Record { fields, .. } = r.def() else {
                panic!("a record")
            };
            fields.iter().map(|f| f.name.clone()).collect()
        };
        assert_eq!(
            names(15),
            [
                "name",
                "statement",
                "prepare_time",
                "parameter_types",
                "from_sql",
                "generic_plans",
                "custom_plans"
            ]
        );
        assert_eq!(names(16)[4], "result_types");
        assert_eq!(names(18).len(), 8);
        assert!(typed("pg_prepared_statements", 18).is_none());
    }
}
