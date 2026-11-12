//! A call of a built-in function in named or mixed notation, as PostgreSQL's parser reads it: its
//! arguments by place first, then each by its parameter's name, put at that parameter's place, and
//! the parameters it leaves out taking their DEFAULTs (`defaults.txt`, each release's
//! `pg_get_expr(proargdefaults, 0)` by the function's OID). The forms are the release's carried
//! pg_proc's, their parameters named by proargnames, the IN ones by proargmodes.

use crate::{Error, Result};
use sqlparser::ast::{Expr, Function, FunctionArg, FunctionArgExpr, FunctionArguments};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};

/// A form of a function whose parameters have names: each IN parameter's name, and the DEFAULTs
/// of its last ones.
#[derive(Clone)]
struct Form {
    names: Vec<String>,
    defaults: Vec<&'static Expr>,
}

/// Each function's DEFAULTs, by its OID.
fn defaults() -> &'static HashMap<u32, Vec<Expr>> {
    static ALL: OnceLock<HashMap<u32, Vec<Expr>>> = OnceLock::new();
    ALL.get_or_init(|| {
        include_str!("../defaults.txt")
            .lines()
            .filter_map(|l| {
                let (oid, list) = l.split_once('\t')?;
                let sql = format!("SELECT {list}");
                let parsed = sqlparser::parser::Parser::parse_sql(
                    &sqlparser::dialect::PostgreSqlDialect {},
                    &sql,
                )
                .ok()?;
                let sqlparser::ast::Statement::Query(q) = parsed.into_iter().next()? else {
                    return None;
                };
                let sqlparser::ast::SetExpr::Select(s) = *q.body else {
                    return None;
                };
                let exprs = s
                    .projection
                    .into_iter()
                    .filter_map(|i| match i {
                        sqlparser::ast::SelectItem::UnnamedExpr(e) => Some(e),
                        _ => None,
                    })
                    .collect();
                Some((oid.parse().ok()?, exprs))
            })
            .collect()
    })
}

/// A text array's elements, as pg_proc's carried rows write them.
fn elements(text: &str) -> Vec<String> {
    let inner = text.trim_start_matches('{').trim_end_matches('}');
    match inner.is_empty() {
        true => Vec::new(),
        false => inner.split(',').map(|e| e.trim_matches('"').to_string()).collect(),
    }
}

/// The forms of a function of that name the release defines whose parameters have names.
fn forms(name: &str, release: u8) -> Vec<Form> {
    static ALL: OnceLock<Mutex<HashMap<u8, std::sync::Arc<HashMap<String, Vec<Form>>>>>> =
        OnceLock::new();
    let all = ALL.get_or_init(Default::default);
    let by_name = {
        let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
        all.entry(release)
            .or_insert_with(|| {
                let field = |r: &HashMap<&'static str, Option<String>>, c: &str| {
                    r.get(c).cloned().flatten().unwrap_or_default()
                };
                let mut out: HashMap<String, Vec<Form>> = HashMap::new();
                for r in crate::pgcatalog::built_in("pg_proc", release) {
                    let names = elements(&field(&r, "proargnames"));
                    if names.is_empty() {
                        continue;
                    }
                    let modes = elements(&field(&r, "proargmodes"));
                    let names: Vec<String> = match modes.is_empty() {
                        true => names,
                        false => names
                            .into_iter()
                            .zip(&modes)
                            .filter(|(_, m)| matches!(m.as_str(), "i" | "b" | "v"))
                            .map(|(n, _)| n)
                            .collect(),
                    };
                    let given: usize = field(&r, "pronargdefaults").parse().unwrap_or(0);
                    let oid: u32 = field(&r, "oid").parse().unwrap_or(0);
                    let defaults = match given {
                        0 => Vec::new(),
                        _ => match defaults().get(&oid) {
                            Some(d) if d.len() == given => d.iter().collect(),
                            _ => continue,
                        },
                    };
                    out.entry(field(&r, "proname"))
                        .or_default()
                        .push(Form { names, defaults });
                }
                std::sync::Arc::new(out)
            })
            .clone()
    };
    by_name.get(name).cloned().unwrap_or_default()
}

/// Whether a call names any of its arguments.
pub(super) fn is_named(f: &Function) -> bool {
    matches!(&f.args, FunctionArguments::List(l) if l.args.iter().any(|a| !matches!(a, FunctionArg::Unnamed(_))))
}

/// A call's arguments as written: each with the name it gives it, where it gives one.
fn written(f: &Function) -> Option<Vec<(Option<String>, &Expr)>> {
    let FunctionArguments::List(l) = &f.args else {
        return None;
    };
    l.args
        .iter()
        .map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => Some((None, x)),
            FunctionArg::Named {
                name,
                arg: FunctionArgExpr::Expr(x),
                ..
            }
            | FunctionArg::ExprNamed {
                name: Expr::Identifier(name),
                arg: FunctionArgExpr::Expr(x),
                ..
            } => Some((Some(crate::catalog::ident(name)), x)),
            _ => None,
        })
        .collect()
}

/// A call in named or mixed notation of a built-in function, its arguments in their parameters'
/// places, those it leaves out their DEFAULTs: none where no form of the release's takes it (the
/// caller refuses it as PostgreSQL does, `no_form`), or where it names none of its arguments.
pub(super) fn positional(f: &Function, release: u8) -> Result<Option<Vec<&Expr>>> {
    if !is_named(f) {
        return Ok(None);
    }
    let Some(given) = written(f) else {
        return Err(Error::new(format!("{f}: its arguments are not read")));
    };
    let name = super::pipeline::function_name(f);
    // An argument by place after one by name is a syntax error in PostgreSQL's grammar.
    if given
        .iter()
        .skip_while(|(n, _)| n.is_none())
        .any(|(n, _)| n.is_none())
    {
        return Err(Error::new(
            "positional argument cannot follow named argument",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for (n, _) in &given {
        if let Some(n) = n {
            if !seen.insert(n.clone()) {
                return Err(Error::new(format!(
                    "argument name \"{n}\" used more than once"
                )));
            }
        }
    }
    for form in forms(&name, release) {
        let n = form.names.len();
        if given.len() > n {
            continue;
        }
        let mut placed: Vec<Option<&Expr>> = vec![None; n];
        let mut fits = true;
        for (i, (named, x)) in given.iter().enumerate() {
            let at = match named {
                None => Some(i),
                Some(p) => form.names.iter().position(|m| m == p),
            };
            match at {
                Some(at) if placed[at].is_none() => placed[at] = Some(*x),
                _ => {
                    fits = false;
                    break;
                }
            }
        }
        // Each parameter left out takes its DEFAULT, the last ones' alone holding one.
        let first_default = n - form.defaults.len();
        let filled: Option<Vec<&Expr>> = placed
            .into_iter()
            .enumerate()
            .map(|(i, x)| x.or_else(|| (i >= first_default).then(|| form.defaults[i - first_default])))
            .collect();
        if let (true, Some(args)) = (fits, filled) {
            return Ok(Some(args));
        }
    }
    Ok(None)
}

/// PostgreSQL's refusal of a call in named notation no form takes: each argument by its type, a
/// named one after its name.
pub(super) fn no_form(f: &Function, types: &[Option<crate::catalog::ColumnType>]) -> Error {
    let names: Vec<Option<String>> = written(f)
        .map(|w| w.into_iter().map(|(n, _)| n).collect())
        .unwrap_or_default();
    let listed: Vec<String> = types
        .iter()
        .zip(names.iter().chain(std::iter::repeat(&None)))
        .map(|(t, n)| {
            let t = super::typed::arg_name(*t);
            match n {
                Some(n) => format!("{n} => {t}"),
                None => t,
            }
        })
        .collect();
    Error::new(format!(
        "function {}({}) does not exist{}",
        crate::catalog::object_name(&f.name),
        listed.join(", "),
        super::typed::NO_FUNCTION
    ))
}

/// Each argument of a call as written, in its order.
pub(super) fn written_args(f: &Function) -> Vec<&Expr> {
    written(f)
        .map(|w| w.into_iter().map(|(_, x)| x).collect())
        .unwrap_or_default()
}
