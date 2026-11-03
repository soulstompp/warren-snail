//! Prepared statements, as PostgreSQL's PREPARE keeps them for its session: each its name, the types
//! of its parameters and its statement, run by EXECUTE with its parameters' values. A parameter's
//! type is the one PREPARE names, or the one the column it meets gives it, or text.

use super::Snail;
use crate::catalog::Catalog;
use crate::{Error, Result};
use sqlparser::ast::{Expr, SetExpr, Statement as Sql};

/// A statement PREPARE keeps.
#[derive(Clone, Debug)]
pub(super) struct Prepared {
    /// Each parameter's type, as PREPARE names it or as where it stands gives it; none where
    /// neither does, and it is read as a literal is, by its context.
    pub(super) types: Vec<Option<String>>,
    /// The statement, as written.
    pub(super) statement: String,
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

/// PREPARE: the statement kept under `name`, each parameter's type named or inferred.
pub(super) fn prepare(
    kept: &mut Kept,
    catalog: &Catalog,
    name: &str,
    given: Vec<Option<String>>,
    body: &str,
) -> Result<()> {
    {
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
        let n = highest(body)?.max(given.len());
        let inferred = inferred(&catalog, body, n);
        let types = (0..n)
            .map(|k| {
                given
                    .get(k)
                    .cloned()
                    .flatten()
                    .or_else(|| inferred[k].clone())
            })
            .collect();
        kept.insert(
            name.to_string(),
            Prepared {
                types,
                statement: body.to_string(),
            },
        );
        Ok(())
    }
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
                "wrong number of parameters for prepared statement \"{name}\""
            )));
        }
        let casings = super::pipeline::Casings::of_file(catalog.monetary, catalog.text_search);
        let mut values = Vec::with_capacity(args.len());
        for (a, ty) in args.iter().zip(&p.types) {
            let text = match ty {
                Some(ty) => format!("CAST(CAST(({a}) AS {ty}) AS text)"),
                None => format!("CAST(({a}) AS text)"),
            };
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
            let v = super::pipeline::eval_on(&e, &[], &[], catalog.release, &casings)?;
            values.push(v.text());
        }
        // Each parameter where it stands: a NULL, a literal of its type where it is a column the
        // statement returns, else a literal its context reads.
        let returned = returned_params(&p.statement);
        let mut out = String::with_capacity(p.statement.len());
        let mut from = 0;
        for (t, a, b) in super::statement::spans(&p.statement)? {
            let sqlparser::tokenizer::Token::Placeholder(ph) = t else {
                continue;
            };
            let Some(k) = ph.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) else {
                continue;
            };
            out.push_str(&p.statement[from..a]);
            match (&values[k - 1], &p.types[k - 1]) {
                (None, Some(ty)) => out.push_str(&format!("CAST(NULL AS {ty})")),
                (None, None) => out.push_str("NULL"),
                (Some(v), Some(ty)) if returned.contains(&k) => {
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
