//! A statement as PostgreSQL's grammar and analyser read it, where sqlparser reads its words
//! otherwise. Before sqlparser parses it: SQL's `SUBSTRING(s SIMILAR p ESCAPE e)` and
//! `SUBSTRING(s FOR e FROM p)` written in the words of the one call PostgreSQL's grammar makes of
//! each, and `BETWEEN SYMMETRIC x` and `BETWEEN ASYMMETRIC x` with their bound in parentheses, which
//! sqlparser reads as a call of the keyword; and a window frame's EXCLUDE, which sqlparser does not
//! read, carried in the window's name (`EXCLUDED`). After: each BETWEEN of a pipeline's statement
//! written as the comparisons PostgreSQL's analyser makes of it.

use crate::{Error, Result};
use sqlparser::ast::{
    AccessExpr, BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArgumentClause,
    FunctionArguments, GroupByExpr, JoinConstraint, JoinOperator, OrderByKind, Query, SelectItem,
    SetExpr, Subscript, TableFactor, UnaryOperator, WindowType,
};
use sqlparser::keywords::Keyword;
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};
use std::borrow::Cow;

/// The statement with each rewrite made, one at a time until none is left.
pub(super) fn spelled(sql: &str) -> Result<Cow<'_, str>> {
    at_at_spaced(sql)?;
    let lower = sql.to_ascii_lowercase();
    let mut out = Cow::Borrowed(sql);
    if lower.contains("exclude") {
        while let Some(next) = excluded_once(&out)? {
            out = Cow::Owned(next);
        }
    }
    if lower.contains("substring") || lower.contains("symmetric") {
        while let Some(next) = rewrite_once(&out)? {
            out = Cow::Owned(next);
        }
    }
    if let Some(next) = crate::grammar::bound(&out) {
        out = Cow::Owned(next);
    }
    Ok(out)
}

/// What stands on the right of `@@@`, which sqlparser reads as `@@` and a prefix `@`.
pub(super) fn right_of_three_ats(right: &Expr) -> Option<&Expr> {
    match right {
        Expr::UnaryOp {
            op: UnaryOperator::PGAbs,
            expr,
        } => Some(expr),
        _ => None,
    }
}

/// `@@` and `@` written apart are two operators to PostgreSQL, and no prefix `@` gives what a `@@`
/// reads; together they are `@@@`, and sqlparser reads the two alike. Refused here, where they can
/// be told apart: a statement printed from what sqlparser read writes `@@@` as `@@ @x`.
pub(super) fn at_at_spaced(sql: &str) -> Result<()> {
    if !sql.contains("@@") {
        return Ok(());
    }
    let tokens = Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize()?;
    for (i, _) in tokens
        .iter()
        .enumerate()
        .filter(|(_, t)| **t == Token::AtAt)
    {
        let next = tokens[i + 1..]
            .iter()
            .position(|t| !matches!(t, Token::Whitespace(_)))
            .map(|n| n + i + 1);
        let apart = |&n: &usize| {
            n > i + 1
                && tokens[n] == Token::AtSign
                && matches!(tokens.get(n + 1), Some(Token::Whitespace(_)))
        };
        if let Some(n) = next.filter(apart) {
            let operand = tokens[n + 1..]
                .iter()
                .find(|t| !matches!(t, Token::Whitespace(_)));
            return Err(Error::new(match operand {
                Some(Token::SingleQuotedString(_)) => {
                    "operator is not unique: @ unknown".to_string()
                }
                _ => "operator does not exist: @@ with a prefix @ on its right".to_string(),
            }));
        }
    }
    Ok(())
}

/// What marks a window's name as carrying its frame's EXCLUDE: the name is this character, the
/// exclusion's words, this character again, and the window it names, if any.
pub(super) const EXCLUDED: char = '\u{1}';

/// The statement with its first frame's EXCLUDE moved into its window's name, or none where no
/// frame excludes: `OVER (w ORDER BY x ROWS … EXCLUDE TIES)` is written
/// `OVER ("\u{1}ties\u{1}w" ORDER BY x ROWS …)`, and `EXCLUDE NO OTHERS`, the default, is dropped.
fn excluded_once(sql: &str) -> Result<Option<String>> {
    let tokens =
        Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize_with_location()?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |l: Location| -> usize {
        let start = starts[l.line as usize - 1];
        start
            + sql[start..]
                .chars()
                .take(l.column as usize - 1)
                .map(char::len_utf8)
                .sum::<usize>()
    };
    let solid: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let word = |i: usize| match solid.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.value.to_ascii_uppercase()),
        _ => None,
    };
    let start = |i: usize| offset(solid[i].span.start);
    let end = |i: usize| offset(solid[i].span.end);
    let mut opened: Vec<usize> = Vec::new();
    for i in 0..solid.len() {
        match solid[i].token {
            Token::LParen => opened.push(i),
            Token::RParen => {
                opened.pop();
            }
            _ => {}
        }
        if word(i).as_deref() != Some("EXCLUDE")
            || !matches!(
                i.checked_sub(1).and_then(word).as_deref(),
                Some("PRECEDING" | "FOLLOWING" | "ROW")
            )
        {
            continue;
        }
        let (kind, last) = match (word(i + 1).as_deref(), word(i + 2).as_deref()) {
            (Some("CURRENT"), Some("ROW")) => (Some("current row"), i + 2),
            (Some("GROUP"), _) => (Some("group"), i + 1),
            (Some("TIES"), _) => (Some("ties"), i + 1),
            (Some("NO"), Some("OTHERS")) => (None, i + 2),
            _ => continue,
        };
        let Some(&open) = opened.last() else {
            continue;
        };
        if !matches!(solid.get(last + 1).map(|t| &t.token), Some(Token::RParen)) {
            continue;
        }
        // The window the spelling names, its identifier folded as PostgreSQL's parser folds it.
        let named = match &solid[open + 1].token {
            Token::Word(w) if w.keyword == Keyword::NoKeyword => Some(match w.quote_style {
                None => w.value.to_ascii_lowercase(),
                Some(_) => w.value.clone(),
            }),
            _ => None,
        };
        let after = if named.is_some() {
            end(open + 1)
        } else {
            end(open)
        };
        let mark = match (kind, named) {
            (Some(k), n) => format!(
                " \"{EXCLUDED}{k}{EXCLUDED}{}\"",
                n.unwrap_or_default().replace('"', "\"\"")
            ),
            (None, Some(n)) => format!(" \"{}\"", n.replace('"', "\"\"")),
            (None, None) => String::new(),
        };
        return Ok(Some(format!(
            "{}{mark}{}{}",
            &sql[..end(open)],
            &sql[after..start(i)],
            &sql[end(last)..]
        )));
    }
    Ok(None)
}

/// A group the words open: a parenthesis, a SUBSTRING call's among them, a bracket, or a CASE.
enum Group {
    Paren,
    Substring(Vec<(Keyword, usize)>),
    Bracket,
    Case,
}

/// The statement with its first rewrite made, or none where it needs none.
fn rewrite_once(sql: &str) -> Result<Option<String>> {
    let tokens =
        Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, sql).tokenize_with_location()?;
    let starts: Vec<usize> = std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let offset = |l: Location| -> usize {
        let start = starts[l.line as usize - 1];
        start
            + sql[start..]
                .chars()
                .take(l.column as usize - 1)
                .map(char::len_utf8)
                .sum::<usize>()
    };
    let solid: Vec<&TokenWithSpan> = tokens
        .iter()
        .filter(|t| !matches!(t.token, Token::Whitespace(_)))
        .collect();
    let keyword = |i: usize| match solid.get(i).map(|t| &t.token) {
        Some(Token::Word(w)) if w.quote_style.is_none() => Some(w.keyword),
        _ => None,
    };
    let start = |i: usize| offset(solid[i].span.start);
    let end = |i: usize| offset(solid[i].span.end);
    let near = |i: usize| match solid.get(i) {
        Some(t) => Error::new(format!("syntax error at or near \"{}\"", t.token)),
        None => Error::new("syntax error at end of input"),
    };
    let mut groups: Vec<Group> = Vec::new();
    for i in 0..solid.len() {
        match &solid[i].token {
            Token::LParen => {
                let call = i > 0 && keyword(i - 1) == Some(Keyword::SUBSTRING);
                groups.push(if call {
                    Group::Substring(Vec::new())
                } else {
                    Group::Paren
                });
            }
            Token::RParen => {
                if let Some(Group::Substring(words)) = groups.pop() {
                    match words.as_slice() {
                        // `SUBSTRING(s SIMILAR p ESCAPE e)` is `SUBSTRING(s FROM p FOR e)`, the
                        // same call of substring of three texts.
                        [(Keyword::SIMILAR, s), rest @ ..] => match rest.first() {
                            Some(&(Keyword::ESCAPE, e)) => {
                                return Ok(Some(format!(
                                    "{}FROM{}FOR{}",
                                    &sql[..start(*s)],
                                    &sql[end(*s)..start(e)],
                                    &sql[end(e)..]
                                )))
                            }
                            Some(&(_, k)) => return Err(near(k)),
                            None => return Err(near(i)),
                        },
                        // `SUBSTRING(s FOR e FROM p)` is `SUBSTRING(s FROM p FOR e)`.
                        [(Keyword::FOR, f), (Keyword::FROM, m), ..] => {
                            return Ok(Some(format!(
                                "{}FROM {} FOR {}{}",
                                &sql[..start(*f)],
                                sql[end(*m)..start(i)].trim(),
                                sql[end(*f)..start(*m)].trim(),
                                &sql[start(i)..]
                            )))
                        }
                        _ => {}
                    }
                }
            }
            Token::LBracket => groups.push(Group::Bracket),
            Token::RBracket => {
                groups.pop();
            }
            _ => {}
        }
        match keyword(i) {
            Some(Keyword::CASE) => groups.push(Group::Case),
            Some(Keyword::END) if matches!(groups.last(), Some(Group::Case)) => {
                groups.pop();
            }
            Some(k @ (Keyword::SIMILAR | Keyword::ESCAPE | Keyword::FOR | Keyword::FROM)) => {
                if let Some(Group::Substring(words)) = groups.last_mut() {
                    if !(k == Keyword::SIMILAR && keyword(i + 1) == Some(Keyword::TO)) {
                        words.push((k, i));
                    }
                }
            }
            Some(Keyword::BETWEEN)
                if matches!(
                    keyword(i + 1),
                    Some(Keyword::SYMMETRIC | Keyword::ASYMMETRIC)
                ) =>
            {
                // The bound runs to the first AND outside any group it opens.
                let first = i + 2;
                let mut depth = 0i32;
                let mut and = None;
                for j in first..solid.len() {
                    match (&solid[j].token, keyword(j)) {
                        (Token::LParen | Token::LBracket, _) | (_, Some(Keyword::CASE)) => {
                            depth += 1
                        }
                        (Token::RParen | Token::RBracket, _) | (_, Some(Keyword::END)) => {
                            depth -= 1;
                            if depth < 0 {
                                break;
                            }
                        }
                        (_, Some(Keyword::AND)) if depth == 0 => {
                            and = Some(j);
                            break;
                        }
                        _ => {}
                    }
                }
                let Some(and) = and.filter(|&a| a > first) else {
                    return Err(near(and.unwrap_or(first)));
                };
                // Already one parenthesis, read as the call.
                let whole = matches!(solid[first].token, Token::LParen)
                    && matches!(solid[and - 1].token, Token::RParen)
                    && {
                        let mut d = 0;
                        (first..and).all(|j| {
                            match solid[j].token {
                                Token::LParen => d += 1,
                                Token::RParen => d -= 1,
                                _ => {}
                            }
                            d > 0 || j == and - 1
                        })
                    };
                if !whole {
                    return Ok(Some(format!(
                        "{}({}) {}",
                        &sql[..start(first)],
                        sql[start(first)..start(and)].trim_end(),
                        &sql[start(and)..]
                    )));
                }
            }
            _ => {}
        }
    }
    Ok(None)
}

/// A pipeline's statement with each BETWEEN written as PostgreSQL's `transformAExprBetween` writes
/// it: `x >= a AND x <= b`; NOT BETWEEN, `x < a OR x > b`; SYMMETRIC, both orders of the bounds
/// or'd; NOT BETWEEN SYMMETRIC, both orders' NOT BETWEEN and'd. Each in parentheses.
pub(super) fn between_written(q: &Query) -> Query {
    let mut q = q.clone();
    query(&mut q);
    q
}

fn query(q: &mut Query) {
    if let Some(w) = &mut q.with {
        for c in &mut w.cte_tables {
            query(&mut c.query);
        }
    }
    set_expr(&mut q.body);
    if let Some(o) = &mut q.order_by {
        if let OrderByKind::Expressions(es) = &mut o.kind {
            for x in es {
                expr(&mut x.expr);
            }
        }
    }
}

fn set_expr(s: &mut SetExpr) {
    match s {
        SetExpr::Select(s) => {
            for item in &mut s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    expr(e);
                }
            }
            for t in &mut s.from {
                table_factor(&mut t.relation);
                for j in &mut t.joins {
                    table_factor(&mut j.relation);
                    if let Some(JoinConstraint::On(e)) = join_constraint(&mut j.join_operator) {
                        expr(e);
                    }
                }
            }
            for e in s.selection.iter_mut().chain(s.having.iter_mut()) {
                expr(e);
            }
            if let GroupByExpr::Expressions(es, _) = &mut s.group_by {
                es.iter_mut().for_each(expr);
            }
        }
        SetExpr::Query(q) => query(q),
        SetExpr::SetOperation { left, right, .. } => {
            set_expr(left);
            set_expr(right);
        }
        _ => {}
    }
}

fn table_factor(t: &mut TableFactor) {
    if let TableFactor::Derived { subquery, .. } = t {
        query(subquery);
    }
}

fn join_constraint(j: &mut JoinOperator) -> Option<&mut JoinConstraint> {
    match j {
        JoinOperator::Join(c)
        | JoinOperator::Inner(c)
        | JoinOperator::Left(c)
        | JoinOperator::LeftOuter(c)
        | JoinOperator::Right(c)
        | JoinOperator::RightOuter(c)
        | JoinOperator::FullOuter(c)
        | JoinOperator::CrossJoin(c) => Some(c),
        _ => None,
    }
}

fn function_arg(a: &mut FunctionArg) {
    let (FunctionArg::Unnamed(x)
    | FunctionArg::Named { arg: x, .. }
    | FunctionArg::ExprNamed { arg: x, .. }) = a;
    if let FunctionArgExpr::Expr(e) = x {
        expr(e);
    }
}

fn function_arguments(a: &mut FunctionArguments) {
    match a {
        FunctionArguments::List(l) => {
            l.args.iter_mut().for_each(function_arg);
            for c in &mut l.clauses {
                match c {
                    FunctionArgumentClause::OrderBy(o) => {
                        o.iter_mut().for_each(|x| expr(&mut x.expr))
                    }
                    FunctionArgumentClause::Where(e) | FunctionArgumentClause::Limit(e) => expr(e),
                    _ => {}
                }
            }
        }
        FunctionArguments::Subquery(q) => query(q),
        FunctionArguments::None => {}
    }
}

/// Each BETWEEN inside `e`, and `e` where it is one, written as its comparisons.
fn expr(e: &mut Expr) {
    let each = |es: &mut [Expr]| es.iter_mut().for_each(expr);
    let boxed = |x: &mut Option<Box<Expr>>| {
        if let Some(x) = x {
            expr(x)
        }
    };
    match e {
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right) => {
            expr(left);
            expr(right);
        }
        Expr::UnaryOp { expr: x, .. }
        | Expr::Nested(x)
        | Expr::Cast { expr: x, .. }
        | Expr::Collate { expr: x, .. }
        | Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotTrue(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x)
        | Expr::Extract { expr: x, .. }
        | Expr::Ceil { expr: x, .. }
        | Expr::Floor { expr: x, .. } => expr(x),
        Expr::InList { expr: x, list, .. } => {
            expr(x);
            each(list);
        }
        Expr::InSubquery {
            expr: x, subquery, ..
        } => {
            expr(x);
            query(subquery);
        }
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => query(q),
        Expr::Like {
            expr: x,
            pattern,
            escape_char,
            ..
        }
        | Expr::ILike {
            expr: x,
            pattern,
            escape_char,
            ..
        }
        | Expr::SimilarTo {
            expr: x,
            pattern,
            escape_char,
            ..
        } => {
            expr(x);
            expr(pattern);
            boxed(escape_char);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            boxed(operand);
            for c in conditions {
                expr(&mut c.condition);
                expr(&mut c.result);
            }
            boxed(else_result);
        }
        Expr::Function(f) => {
            function_arguments(&mut f.args);
            function_arguments(&mut f.parameters);
            boxed(&mut f.filter);
            f.within_group.iter_mut().for_each(|x| expr(&mut x.expr));
            if let Some(WindowType::WindowSpec(w)) = &mut f.over {
                each(&mut w.partition_by);
                w.order_by.iter_mut().for_each(|x| expr(&mut x.expr));
            }
        }
        Expr::Substring {
            expr: x,
            substring_from,
            substring_for,
            ..
        } => {
            expr(x);
            boxed(substring_from);
            boxed(substring_for);
        }
        Expr::Trim {
            expr: x,
            trim_what,
            trim_characters,
            ..
        } => {
            expr(x);
            boxed(trim_what);
            if let Some(cs) = trim_characters {
                each(cs);
            }
        }
        Expr::Position { expr: x, r#in } => {
            expr(x);
            expr(r#in);
        }
        Expr::Overlay {
            expr: x,
            overlay_what,
            overlay_from,
            overlay_for,
        } => {
            expr(x);
            expr(overlay_what);
            expr(overlay_from);
            boxed(overlay_for);
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            expr(timestamp);
            expr(time_zone);
        }
        Expr::Tuple(es) => each(es),
        Expr::Array(a) => each(&mut a.elem),
        Expr::CompoundFieldAccess { root, access_chain } => {
            expr(root);
            for a in access_chain {
                match a {
                    AccessExpr::Dot(x) => expr(x),
                    AccessExpr::Subscript(Subscript::Index { index }) => expr(index),
                    AccessExpr::Subscript(Subscript::Slice {
                        lower_bound,
                        upper_bound,
                        stride,
                    }) => {
                        for x in [lower_bound, upper_bound, stride].into_iter().flatten() {
                            expr(x);
                        }
                    }
                }
            }
        }
        Expr::Interval(i) => expr(&mut i.value),
        Expr::Between {
            expr: x, low, high, ..
        } => {
            expr(x);
            expr(low);
            expr(high);
        }
        _ => {}
    }
    if let Expr::Between {
        expr: x,
        negated,
        low,
        high,
    } = e
    {
        let (low, symmetric) = super::question::between_low(low);
        let cmp = |op: BinaryOperator, bound: &Expr| Expr::BinaryOp {
            left: x.clone(),
            op,
            right: Box::new(bound.clone()),
        };
        let join = |a: Expr, op: BinaryOperator, b: Expr| Expr::BinaryOp {
            left: Box::new(Expr::Nested(Box::new(a))),
            op,
            right: Box::new(Expr::Nested(Box::new(b))),
        };
        use BinaryOperator::{And, Gt, GtEq, Lt, LtEq, Or};
        let within = |a: &Expr, b: &Expr| join(cmp(GtEq, a), And, cmp(LtEq, b));
        let outside = |a: &Expr, b: &Expr| join(cmp(Lt, a), Or, cmp(Gt, b));
        let written = match (*negated, symmetric) {
            (false, false) => within(low, high),
            (true, false) => outside(low, high),
            (false, true) => join(within(low, high), Or, within(high, low)),
            (true, true) => join(outside(low, high), And, outside(high, low)),
        };
        *e = Expr::Nested(Box::new(written));
    }
}
