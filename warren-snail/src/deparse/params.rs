//! A statement's parameters, `$1` and on, typed as PostgreSQL's parse analysis types them for
//! PREPARE and for a protocol's Parse (parse_param.c): each unknown where it is first met, given
//! the type its first coercion asks for, every later reference that type; refused where its
//! reading leaves it unknown, or reads it as two types.

use super::analyze::Node;
use super::cat::{self, Cat};
use super::query::{Query, RteKind};
use crate::catalog::Catalog;
use crate::{Error, Result};
use std::cell::RefCell;

/// The most parameters a statement is read with.
const MOST: usize = 65535;

thread_local! {
    /// The types of the parameters a statement's reading is settling, by number from 1: 0 for one
    /// not met yet, unknown for one met and not yet settled; none while no statement's parameters
    /// are being typed.
    static PARAMS: RefCell<Option<Vec<u32>>> = const { RefCell::new(None) };
}

/// A reference to `$n`, as variable_paramref_hook makes it: a parameter of the type settled for
/// it so far. None where no statement's parameters are being typed.
pub(crate) fn reference(p: &str) -> Option<Result<Node>> {
    PARAMS.with(|ps| {
        let mut ps = ps.borrow_mut();
        let slots = ps.as_mut()?;
        let n = match p.strip_prefix('$').map(str::parse::<usize>) {
            Some(Ok(n)) if (1..=MOST).contains(&n) => n,
            Some(_) => return Some(Err(Error::new(format!("there is no parameter {p}")))),
            None => return None,
        };
        if slots.len() < n {
            slots.resize(n, 0);
        }
        if slots[n - 1] == 0 {
            slots[n - 1] = cat::UNKNOWN;
        }
        Some(Ok(Node::Param {
            n,
            ty: slots[n - 1],
        }))
    })
}

/// A parameter not yet settled coerced to `target`, as variable_coerce_param_hook takes it: of
/// that type, and every later reference to it; refused where an earlier coercion settled it as
/// another.
pub(crate) fn settled(cat: &Cat, n: usize, target: u32) -> Result<Node> {
    let before = PARAMS.with(|ps| {
        let mut ps = ps.borrow_mut();
        let slot = ps.as_mut().and_then(|s| s.get_mut(n.wrapping_sub(1)))?;
        let before = *slot;
        if before == cat::UNKNOWN {
            *slot = target;
        }
        Some(before)
    });
    match before {
        None => Err(Error::new(format!("there is no parameter ${n}"))),
        Some(b) if b == cat::UNKNOWN || b == target => Ok(Node::Param { n, ty: target }),
        Some(b) => Err(Error::new(format!(
            "inconsistent types deduced for parameter ${n}: {} versus {}",
            super::print::format_type_be(cat, b),
            super::print::format_type_be(cat, target)
        ))),
    }
}

/// The type settled for `$n` so far: unknown where none is, 0 where it is not met.
pub(crate) fn type_of(n: usize) -> u32 {
    PARAMS.with(|ps| {
        ps.borrow()
            .as_ref()
            .and_then(|s| s.get(n.wrapping_sub(1)).copied())
            .unwrap_or(0)
    })
}

/// A statement's parameters being typed, from the types given for the first of them, until
/// dropped.
struct Typing(Option<Vec<u32>>);

impl Typing {
    fn of(given: Vec<u32>) -> Typing {
        Typing(PARAMS.with(|ps| ps.replace(Some(given))))
    }

    fn types(&self) -> Vec<u32> {
        PARAMS.with(|ps| ps.borrow().clone().unwrap_or_default())
    }
}

impl Drop for Typing {
    fn drop(&mut self) {
        let before = self.0.take();
        PARAMS.with(|ps| *ps.borrow_mut() = before);
    }
}

/// Each parameter a read statement holds, by its number and the type it holds there, as
/// query_tree_walker reaches it: a sort's or a grouping's expression is its target's, no other.
fn each_param(q: &Query, f: &mut dyn FnMut(usize, u32)) {
    for t in &q.targets {
        each_in(&t.expr, f);
    }
    for n in q.quals.iter().chain(&q.having).chain(&q.offset).chain(&q.limit).chain(&q.assigned) {
        each_in(n, f);
    }
    for j in &q.from {
        join_quals(j, f);
    }
    for w in &q.windows {
        if let Some(fr) = &w.frame {
            for b in std::iter::once(&fr.start).chain(&fr.end) {
                if let super::query::Bound::Preceding(n) | super::query::Bound::Following(n) = b {
                    each_in(n, f);
                }
            }
        }
    }
    for r in &q.rtable {
        match &r.kind {
            RteKind::Subquery(sub) => each_param(sub, f),
            RteKind::Values { rows } => rows.iter().flatten().for_each(|n| each_in(n, f)),
            _ => {}
        }
    }
    for c in &q.ctes {
        each_param(&c.query, f);
    }
}

fn join_quals(j: &super::query::Jt, f: &mut dyn FnMut(usize, u32)) {
    if let super::query::Jt::Join(j) = j {
        join_quals(&j.left, f);
        join_quals(&j.right, f);
        if let Some(n) = &j.quals {
            each_in(n, f);
        }
    }
}

fn each_in(n: &Node, f: &mut dyn FnMut(usize, u32)) {
    n.walk(&mut |x| match x {
        Node::Param { n, ty } => f(*n, *ty),
        Node::SubLink { query, .. } => each_param(query, f),
        _ => {}
    });
}

/// The type each parameter of a statement takes, as PostgreSQL's parse analysis gives it, the
/// first `given` named (0 or unknown where PREPARE names none): refused as PostgreSQL refuses
/// where its reading cannot settle one. Where `checked` is false, a parameter left unknown is
/// unknown, and none is refused for it.
pub(crate) fn types(catalog: &Catalog, sql: &str, given: &[u32], checked: bool) -> Result<Vec<u32>> {
    reading(catalog, sql, given, checked).map(|r| r.params)
}

/// What PostgreSQL's parse analysis of a statement gives a prepared statement: each parameter's
/// type, and each column's of the rows it returns, none where it returns no rows (a write without
/// RETURNING), as its result descriptor has them.
pub(crate) struct Reading {
    pub(crate) params: Vec<u32>,
    pub(crate) results: Option<Vec<u32>>,
}

/// A statement read as `types` reads it, with the types of the columns it returns.
pub(crate) fn reading(catalog: &Catalog, sql: &str, given: &[u32], checked: bool) -> Result<Reading> {
    let bound = crate::grammar::bound(sql);
    let parsed = crate::read::image::parse_sql(bound.as_deref().unwrap_or(sql))?;
    let [st] = parsed.as_slice() else {
        return Err(Error::new("a prepared statement is one statement"));
    };
    let cat = Cat::of(catalog);
    let given = given
        .iter()
        .map(|&t| if t == 0 { cat::UNKNOWN } else { t })
        .collect();
    let typing = Typing::of(given);
    let read = super::query::statement(&cat, st)?;
    let slots = typing.types();
    drop(typing);
    use sqlparser::ast::{SetExpr, Statement};
    let writes = match st {
        Statement::Insert(_) | Statement::Update(_) | Statement::Delete(_) => true,
        Statement::Query(q) => matches!(
            q.body.as_ref(),
            SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_)
        ),
        _ => false,
    };
    let outputs: Vec<u32> = read.output().map(|t| t.expr.ty()).collect();
    let results = match writes && outputs.is_empty() {
        true => None,
        false => Some(outputs),
    };
    if !checked {
        return Ok(Reading { params: slots, results });
    }
    // check_variable_parameters: a reference left unknown where another settled its parameter.
    let mut left = None;
    each_param(&read, &mut |n, ty| {
        if left.is_none() && slots.get(n - 1).is_some_and(|&s| s != ty) {
            left = Some(n);
        }
    });
    if let Some(n) = left {
        return Err(Error::coded(
            "42P08",
            format!("could not determine data type of parameter ${n}"),
        ));
    }
    // pg_analyze_and_rewrite_varparams: every parameter settled.
    if let Some(k) = slots.iter().position(|&t| t == 0 || t == cat::UNKNOWN) {
        return Err(Error::new(format!(
            "could not determine data type of parameter ${}",
            k + 1
        )));
    }
    Ok(Reading { params: slots, results })
}

/// Whether a refusal of a statement's reading is PostgreSQL's own for its parameters, in its
/// words: a parameter never settled or settled two ways, an operator or a function its unknowns
/// leave unchosen, an operator its settled types find none of, a value its column cannot take.
/// Any other is the reading's, and the statement is read as before.
pub(crate) fn refuses(e: &Error) -> bool {
    matches!(e.sqlstate(), Some("42P18" | "42P08" | "42P02" | "42725" | "42804"))
        || e.0.starts_with("operator does not exist: ")
}

/// A statement's text with each reference to a parameter written as a parameter of its own,
/// numbered in the order they stand, and the parameter each stands for.
pub(crate) fn each_reference(sql: &str) -> Result<(String, Vec<usize>)> {
    let mut out = String::with_capacity(sql.len());
    let mut refs = Vec::new();
    let mut from = 0;
    for (t, a, b) in crate::read::statement::spans(sql)? {
        let sqlparser::tokenizer::Token::Placeholder(p) = t else {
            continue;
        };
        let Some(k) = p.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) else {
            continue;
        };
        refs.push(k);
        out.push_str(&sql[from..a]);
        out.push_str(&format!("${}", refs.len()));
        from = b;
    }
    out.push_str(&sql[from..]);
    Ok((out, refs))
}

/// EvaluateParams: an EXECUTE's `k`th argument read, as PostgreSQL reads it with no parameters
/// of its own and no subquery, and refused where an assignment cannot coerce it to `expected`.
pub(crate) fn argument(catalog: &Catalog, k: usize, arg: &str, expected: u32) -> Result<()> {
    for (t, _, _) in crate::read::statement::spans(arg)? {
        if let sqlparser::tokenizer::Token::Placeholder(p) = t {
            return Err(Error::new(format!("there is no parameter {p}")));
        }
    }
    let parsed = crate::read::image::parse_sql(&format!("SELECT ({arg})"))?;
    let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
        return Ok(());
    };
    let sqlparser::ast::SetExpr::Select(s) = q.body.as_ref() else {
        return Ok(());
    };
    let Some(sqlparser::ast::SelectItem::UnnamedExpr(e)) = s.projection.first() else {
        return Ok(());
    };
    use sqlparser::ast::Expr as E;
    let mut sub = false;
    let mut named: Option<String> = None;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        sub |= matches!(x, E::Subquery(_) | E::Exists { .. } | E::InSubquery { .. });
        // An argument reads no relation, so no column.
        if named.is_none() {
            named = match x {
                E::Identifier(i) => Some(format!("column \"{}\" does not exist", super::analyze::folded(i))),
                E::CompoundIdentifier(p) if p.len() >= 2 => Some(format!(
                    "missing FROM-clause entry for table \"{}\"",
                    super::analyze::folded(&p[p.len() - 2])
                )),
                _ => None,
            };
        }
        std::ops::ControlFlow::<()>::Continue(())
    });
    if sub {
        return Err(Error::new("cannot use subquery in EXECUTE parameter"));
    }
    if let Some(n) = named {
        return Err(Error::new(n));
    }
    if expected == 0 {
        return Ok(());
    }
    let cat = Cat::of(catalog);
    let a = super::analyze::Analyzer {
        cat: &cat,
        columns: Vec::new(),
        value: None,
        scope: None,
    };
    // An argument the reading does not read is evaluated as before.
    let Ok(n) = a.expr(e) else {
        return Ok(());
    };
    let given = n.ty();
    if a.can_coerce_all(&[given], &[expected], super::analyze::Context::Assignment) {
        return Ok(());
    }
    Err(Error::new(format!(
        "parameter ${k} of type {} cannot be coerced to the expected type {}",
        super::print::format_type_be(&cat, given),
        super::print::format_type_be(&cat, expected)
    )))
}

/// Each type's name as format_type_be writes it, and as regtype's output does.
pub(crate) fn type_names(catalog: &Catalog, oids: &[u32]) -> Vec<String> {
    let cat = Cat::of(catalog);
    oids.iter().map(|&t| super::print::format_type_be(&cat, t)).collect()
}

/// A type named as PREPARE's list names one, as typenameTypeId reads it: its OID, any typmod
/// dropped; unknown for `unknown`.
pub(crate) fn named_type(catalog: &Catalog, name: &str) -> Result<u32> {
    crate::regs::reg_parse(crate::regs::RegKind::Type, name, catalog.release).map_err(|e| {
        // PostgreSQL's words alone, without where the snail looked.
        let end = "\" does not exist";
        match (e.0.starts_with("type \""), e.0.find(end)) {
            (true, Some(at)) => Error::new(&e.0[..at + end.len()]),
            _ => e,
        }
    })
}

#[cfg(test)]
mod tests {
    use crate::catalog::Catalog;

    fn typed(c: &Catalog, sql: &str) -> String {
        match super::types(c, sql, &[], true) {
            Ok(t) => super::type_names(c, &t).join(","),
            Err(e) => format!("{} {}", e.sqlstate().unwrap_or("-----"), e.0),
        }
    }

    #[test]
    fn each_parameter_is_typed_where_postgresql_settles_it() {
        let c = Catalog::parse(
            "CREATE SCHEMA a; CREATE TABLE a.t (k integer NOT NULL, a varchar(5), n numeric(6,2), \
             PRIMARY KEY (k));",
        )
        .unwrap();
        for (sql, want) in [
            (
                "UPDATE a.t SET n = coalesce(n, 0) + $1 WHERE k = $2 RETURNING n * $3",
                "numeric,integer,numeric",
            ),
            (
                "INSERT INTO a.t (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET n = \
                 coalesce(excluded.n, 0) + $2 WHERE a.t.a = $3",
                "integer,numeric,text",
            ),
            ("SELECT k FROM a.t WHERE (k, a) = ($1, $2)", "integer,text"),
        ] {
            assert_eq!(typed(&c, sql), want, "{sql}");
        }
    }
}
