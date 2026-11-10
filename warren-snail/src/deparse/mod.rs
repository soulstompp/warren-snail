//! PostgreSQL's deparsers over a snail's catalog: `pg_get_expr` and the definitions of its
//! constraints, indexes, views, rules and functions, each printed as PostgreSQL 18's ruleutils.c
//! prints the same object. An expression is read as PostgreSQL's parser reads it, every operator,
//! function and cast resolved over the catalog of the file's release (`analyze`), then printed
//! (`print`). A catalog column holding a parse tree carries a name of the object it comes from,
//! which only `pg_get_expr` reads.

pub(crate) mod analyze;
pub(crate) mod cat;
pub(crate) mod defs;
pub(crate) mod depend;
pub(crate) mod fold;
pub(crate) mod funcs;
pub(crate) mod params;
pub(crate) mod print;
pub(crate) mod query;
pub(crate) mod rewrap;
pub(crate) mod viewdef;

use crate::catalog::{Catalog, Table};
use crate::oids::{full, Object};
use crate::read::pipeline::Value;
use crate::{Error, Result};
use analyze::{Analyzer, Column, Context, Form, Node};
use cat::Cat;
use print::Printer;
use std::cell::RefCell;
use std::sync::Arc;

thread_local! {
    /// The catalog of the file a round reads, while a statement that may call a deparser runs.
    static HELD: RefCell<Option<Arc<Catalog>>> = const { RefCell::new(None) };
}

/// The file's catalog held for the deparsers until dropped.
pub(crate) struct Holding(Option<Option<Arc<Catalog>>>);

/// Holds a file's catalog for the round's deparsers, where its statement names the catalog or calls
/// one of them.
pub(crate) fn holding(catalog: &Catalog, sql: &str) -> Holding {
    let lower = sql.to_ascii_lowercase();
    if !lower.contains("pg_get_") && !lower.contains("_is_updatable") && !crate::pgcatalog::names_catalog(sql) {
        return Holding(None);
    }
    let before = HELD.with(|h| h.replace(Some(Arc::new(catalog.clone()))));
    Holding(Some(before))
}

impl Drop for Holding {
    fn drop(&mut self) {
        if let Some(before) = self.0.take() {
            HELD.with(|h| *h.borrow_mut() = before);
        }
    }
}

pub(crate) fn held() -> Result<Arc<Catalog>> {
    HELD.with(|h| h.borrow().clone()).ok_or_else(|| {
        Error::new("a deparser reads one snail's catalog, and this round holds none")
    })
}

/// The deparsers a pipeline answers, by name.
pub(crate) fn is_deparser(name: &str) -> bool {
    matches!(
        name,
        "pg_get_expr"
            | "pg_get_constraintdef"
            | "pg_get_indexdef"
            | "pg_get_viewdef"
            | "pg_get_ruledef"
            | "pg_get_triggerdef"
    ) || funcs::is_function_printer(name)
}

/// What a deparser returns.
pub(crate) fn typed(name: &str) -> Option<crate::catalog::ColumnType> {
    is_deparser(name).then_some(crate::catalog::ColumnType::Text(None))
}

/// An OID as an argument gives it: an oid's or a reg* value's own, or a number written out.
pub(crate) fn oid_of(v: &Value) -> Option<u32> {
    use crate::catalog::ColumnType as T;
    if let Value::Scalar(ty @ (T::Oid | T::Reg(..)), k) = v {
        return u32::try_from(crate::scalars::raw_of_key(*ty, *k)).ok();
    }
    let t = v.text()?;
    t.parse::<i64>().ok().and_then(|n| u32::try_from(n).ok())
}

fn truth_of(v: &Value) -> Option<bool> {
    match v.text()?.as_str() {
        "t" | "true" => Some(true),
        "f" | "false" => Some(false),
        _ => None,
    }
}

/// A deparser called: its answer, NULL where PostgreSQL's is.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    if !is_deparser(name) {
        return Ok(None);
    }
    if args.iter().take(2).any(|v| matches!(v, Value::Null)) {
        return Ok(Some(Value::Null));
    }
    let catalog = held()?;
    let cat = Cat::of(&catalog);
    let text = match name {
        _ if funcs::is_function_printer(name) => {
            let oid = args.first().and_then(oid_of).unwrap_or(0);
            let nth = args.get(1).and_then(Value::text).and_then(|s| s.parse::<i64>().ok());
            funcs::printed(&cat, name, oid, nth)?
        }
        "pg_get_expr" => {
            let tree = match args.first() {
                Some(Value::Typed(_, s)) => s.clone(),
                Some(v) => v.text().unwrap_or_default(),
                None => String::new(),
            };
            let relid = args.get(1).and_then(oid_of).unwrap_or(0);
            let pretty = args.get(2).and_then(truth_of).unwrap_or(false);
            expr_of(&cat, &tree, relid, pretty)?
        }
        "pg_get_constraintdef" => {
            let oid = args.first().and_then(oid_of).unwrap_or(0);
            let pretty = args.get(1).and_then(truth_of).unwrap_or(false);
            defs::constraintdef(&cat, oid, pretty)?
        }
        "pg_get_indexdef" => {
            let oid = args.first().and_then(oid_of).unwrap_or(0);
            let colno = args.get(1).and_then(Value::text).and_then(|s| s.parse::<i64>().ok()).unwrap_or(0);
            let pretty = args.get(2).and_then(truth_of).unwrap_or(false);
            defs::indexdef(&cat, oid, colno.max(0) as usize, pretty)?
        }
        "pg_get_ruledef" => {
            let oid = args.first().and_then(oid_of).unwrap_or(0);
            let pretty = args.get(1).and_then(truth_of).unwrap_or(false);
            viewdef::ruledef(&cat, oid, pretty)?
        }
        "pg_get_triggerdef" => {
            let oid = args.first().and_then(oid_of).unwrap_or(0);
            let pretty = args.get(1).and_then(truth_of).unwrap_or(false);
            defs::triggerdef(&cat, oid, pretty)?
        }
        "pg_get_viewdef" => {
            let pretty_or_wrap = args.get(1);
            let (pretty, wrap) = match pretty_or_wrap.and_then(truth_of) {
                Some(p) => (p, 0),
                None => match pretty_or_wrap.and_then(Value::text).and_then(|s| s.parse::<i32>().ok()) {
                    Some(w) => (true, w),
                    None => (false, 0),
                },
            };
            match view_oid(&catalog, &args[0])? {
                Some(oid) => viewdef::viewdef(&cat, oid, pretty, wrap)?,
                None => None,
            }
        }
        _ => None,
    };
    Ok(Some(text.map_or(Value::Null, Value::Text)))
}

/// The name a catalog column carries in place of a parse tree: the object, and which of its trees.
pub(crate) fn tree_handle(kind: &str, oid: u32, part: i64) -> String {
    format!("snail:{kind}:{oid}:{part}")
}

/// A table of the file by its OID.
fn table_of(catalog: &Catalog, oid: u32) -> Option<&Table> {
    match catalog.oids.object(oid)? {
        Object::Class(n) => catalog.tables.iter().find(|t| full(&t.name) == *n),
        _ => None,
    }
}

/// A table's columns as an expression over it reads them.
pub(crate) fn columns_of(cat: &Cat, t: &Table) -> Vec<Column> {
    t.columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let (ty, typmod) = column_type(cat, c);
            Column {
                name: c.name.clone(),
                attno: i as i16 + 1,
                ty,
                typmod,
            }
        })
        .collect()
}

/// A column's type and typmod, as pg_attribute gives them.
pub(crate) fn column_type(cat: &Cat, c: &crate::catalog::Column) -> (u32, i32) {
    let oid = crate::pgcatalog::type_oid_of(cat.catalog, c.ty, c.domain.as_deref());
    let typmod = if c.domain.is_some() {
        -1
    } else {
        crate::pgcatalog::typmod_of(c.ty)
    };
    (oid, typmod)
}

/// The names of a relation's columns, by number.
fn names_of(catalog: &Catalog, relid: u32) -> Option<Vec<String>> {
    if let Some(t) = table_of(catalog, relid) {
        return Some(t.columns.iter().map(|c| c.name.clone()).collect());
    }
    match catalog.oids.object(relid)? {
        Object::Class(n) => catalog
            .views
            .iter()
            .find(|v| full(&v.name) == *n)
            .map(|v| v.columns.clone()),
        _ => None,
    }
}

/// A composite's fields: each name, type and typmod; those of a table's row type its columns'.
pub(crate) fn row_fields(cat: &Cat, relid: u32) -> Vec<(String, u32, i32)> {
    if let Some(t) = table_of(cat.catalog, relid) {
        return columns_of(cat, t)
            .into_iter()
            .map(|c| (c.name, c.ty, c.typmod))
            .collect();
    }
    // A composite of the file's, its fields as its DDL names their types.
    if let Some(Object::Class(n)) = cat.catalog.oids.object(relid) {
        for t in &cat.catalog.types {
            if let crate::catalog::TypeKind::Composite { fields } = &t.kind {
                if full(&t.name) == *n {
                    return fields
                        .iter()
                        .map(|(name, ty, _)| {
                            let oid = crate::regs::type_oid(ty, cat.release).unwrap_or(0);
                            let typmod = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
                                .try_with_sql(ty)
                                .and_then(|mut p| p.parse_data_type())
                                .ok()
                                .and_then(|dt| crate::catalog::held_type(&dt).ok())
                                .map_or(-1, crate::pgcatalog::typmod_of);
                            (name.clone(), oid, typmod)
                        })
                        .collect();
                }
            }
        }
    }
    // A built-in relation's columns are the carried ones; only the file's own read its rows.
    let own = match relid >= crate::oids::FIRST {
        true => crate::pgcatalog::file_rows(cat.catalog, "pg_attribute"),
        false => Vec::new(),
    };
    let mut fields: Vec<(i16, String, u32, i32)> = own
        .into_iter()
        .chain(crate::pgcatalog::built_in("pg_attribute", cat.release))
        .filter(|r| r.get("attrelid").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(relid))
        .filter_map(|r| {
            let num: i16 = r.get("attnum")?.as_deref()?.parse().ok()?;
            (num > 0).then_some(())?;
            Some((
                num,
                r.get("attname")?.clone()?,
                r.get("atttypid")?.as_deref()?.parse().ok()?,
                r.get("atttypmod")?.as_deref()?.parse().unwrap_or(-1),
            ))
        })
        .collect();
    fields.sort_by_key(|f| f.0);
    fields.into_iter().map(|f| (f.1, f.2, f.3)).collect()
}

/// A range type's subtype.
pub(crate) fn range_subtype(cat: &Cat, t: u32) -> Option<u32> {
    // A built-in range's row is carried; only the file's own read its rows.
    let rows = || {
        let own = match t >= crate::oids::FIRST {
            true => crate::pgcatalog::file_rows(cat.catalog, "pg_range"),
            false => Vec::new(),
        };
        crate::pgcatalog::built_in("pg_range", cat.release).into_iter().chain(own)
    };
    let mut t = t;
    if let Some(r) = cat.ty(t) {
        if r.typtype == 'm' {
            t = rows()
                .find(|r| r.get("rngmultitypid").cloned().flatten().and_then(|s| s.parse().ok()) == Some(t))
                .and_then(|r| r.get("rngtypid").cloned().flatten()?.parse().ok())?;
        }
    }
    rows()
        .find(|r| r.get("rngtypid").cloned().flatten().and_then(|s| s.parse().ok()) == Some(t))
        .and_then(|r| r.get("rngsubtype").cloned().flatten()?.parse().ok())
}

/// A literal's text as a type's input then output give it: what a constant of that type prints.
pub(crate) fn constant_text(cat: &Cat, lit: &str, ty: u32, typmod: i32) -> Result<String> {
    if ty == cat::UNKNOWN || ty == cat::TEXT {
        return Ok(lit.to_string());
    }
    // An array of a domain is written as an array of its base, as a domain writes its base's
    // output.
    let (ty, typmod) = match cat.element(ty) {
        e if e != 0 && cat.base(e) != e && cat.array_of(cat.base(e)) != 0 => {
            (cat.array_of(cat.base(e)), -1)
        }
        _ => (ty, typmod),
    };
    let name = print::format_type(cat, ty, typmod);
    let sql = format!(
        "CAST(CAST({} AS {name}) AS text)",
        print::literal(lit)
    );
    let e = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
        .try_with_sql(&sql)
        .and_then(|mut p| p.parse_expr())
        .map_err(|err| Error::new(format!("{sql}: {err}")))?;
    let casings = crate::read::pipeline::Casings::of_file(cat.catalog.monetary, cat.catalog.text_search);
    match crate::read::pipeline::eval_on(&e, &[], &[], cat.release, &casings)? {
        Value::Null => Ok(lit.to_string()),
        v => v.text().ok_or_else(|| Error::new(format!("{sql} gave no text"))),
    }
}

/// Parses an expression a catalog keeps as text.
pub(crate) fn parse_expr(text: &str) -> Result<sqlparser::ast::Expr> {
    let spelled = crate::read::spelled::spelled(text)?;
    sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
        .try_with_sql(&spelled)
        .and_then(|mut p| p.parse_expr())
        .map_err(|err| Error::new(format!("{text}: {err}")))
}

/// The tree a handle names, as the parser left it, and the relation its columns are read from.
fn tree_of(cat: &Cat, handle: &str) -> Result<(Node, u32)> {
    let bad = || Error::new("pg_get_expr reads a parse tree a snail's catalog gave");
    let mut parts = handle.split(':');
    if parts.next() != Some("snail") {
        return Err(bad());
    }
    let kind = parts.next().ok_or_else(bad)?;
    let oid: u32 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let part: i64 = parts.next().and_then(|s| s.parse().ok()).ok_or_else(bad)?;
    let catalog = cat.catalog;
    match kind {
        // A column's DEFAULT or generation, coerced to the column's type as cookDefault does.
        "attrdef" => {
            let t = table_of(catalog, oid).ok_or_else(bad)?;
            let c = t.columns.get((part - 1) as usize).ok_or_else(bad)?;
            let raw = c
                .default
                .as_ref()
                .or(c.generated.as_ref().map(|g| &g.expr))
                .ok_or_else(bad)?;
            let a = Analyzer {
                cat,
                columns: columns_of(cat, t),
                value: None,
                scope: None,
            };
            let (ty, typmod) = column_type(cat, c);
            let n = a.expr(raw)?;
            Ok((a.coerce_to_target(n, ty, typmod, Context::Assignment, Form::Implicit)?, oid))
        }
        // A table's CHECK, coerced to boolean.
        "check" => {
            let t = table_of(catalog, oid).ok_or_else(bad)?;
            let k = t.checks.get(part as usize).ok_or_else(bad)?;
            let a = Analyzer {
                cat,
                columns: columns_of(cat, t),
                value: None,
                scope: None,
            };
            Ok((a.boolean(a.expr(&k.expr)?)?, oid))
        }
        // A domain's CHECK, its VALUE of the domain's base type.
        "domaincheck" => {
            let name = match catalog.oids.object(oid) {
                Some(Object::Type(n)) => n.clone(),
                _ => return Err(bad()),
            };
            let d = catalog
                .domains
                .iter()
                .find(|d| full(&d.name) == name)
                .ok_or_else(bad)?;
            let k = d.checks.get(part as usize).ok_or_else(bad)?;
            let (base, typmod) = cat.base_and_typmod(oid, -1);
            let a = Analyzer {
                cat,
                columns: Vec::new(),
                value: Some((base, typmod)),
                scope: None,
            };
            Ok((a.boolean(a.expr(&k.check)?)?, 0))
        }
        // A domain's DEFAULT, coerced to the domain.
        "domaindefault" => {
            let name = match catalog.oids.object(oid) {
                Some(Object::Type(n)) => n.clone(),
                _ => return Err(bad()),
            };
            let d = catalog
                .domains
                .iter()
                .find(|d| full(&d.name) == name)
                .ok_or_else(bad)?;
            let raw = d.default.as_ref().ok_or_else(bad)?;
            let a = Analyzer {
                cat,
                columns: Vec::new(),
                value: None,
                scope: None,
            };
            let n = a.expr(raw)?;
            Ok((a.coerce_to_target(n, oid, -1, Context::Assignment, Form::Implicit)?, 0))
        }
        // An index's key expressions, as one list, and its predicate.
        "indexprs" | "indpred" => {
            let (exprs, pred, rel) = defs::index_trees(cat, oid)?.ok_or_else(bad)?;
            let n = if kind == "indexprs" { exprs } else { pred };
            Ok((n.ok_or_else(bad)?, rel))
        }
        _ => Err(bad()),
    }
}

/// The tree a stored expression of an object holds, as the parser left it: `attrdef`, `check`,
/// `domaincheck`, `indexprs` or `indpred` by its handle's parts; a domain's DEFAULT coerced to the
/// domain's base type, as DefineDomain cooks it.
pub(crate) fn stored_tree(cat: &Cat, kind: &str, oid: u32, part: i64) -> Result<Node> {
    let (node, _) = tree_of(cat, &tree_handle(kind, oid, part))?;
    Ok(match node {
        Node::Domain { arg, ty, .. } if kind == "domaindefault" && ty == oid => *arg,
        n => n,
    })
}

/// pg_get_expr: the tree a handle names, printed with its columns named by `relid`'s.
fn expr_of(cat: &Cat, handle: &str, relid: u32, pretty: bool) -> Result<Option<String>> {
    let (node, _) = tree_of(cat, handle)?;
    let relations = if relid != 0 {
        match names_of(cat.catalog, relid) {
            Some(names) => vec![(names, None)],
            None => return Ok(None),
        }
    } else {
        if node.has_vars() {
            return Err(Error::new("expression contains variables"));
        }
        Vec::new()
    };
    let mut p = Printer {
        cat,
        paren: pretty,
        indent: true,
        level: 0,
        buf: String::new(),
        relations,
        q: Default::default(),
    };
    p.expr(&node, false)?;
    Ok(Some(p.buf))
}

/// The text a parse tree's handle prints, as pg_get_expr prints it: what pg_type's typdefault
/// holds of a domain's DEFAULT.
pub(crate) fn text_of_tree(catalog: &Catalog, handle: &str, relid: u32, pretty: bool) -> Result<Option<String>> {
    let cat = Cat::of(catalog);
    expr_of(&cat, handle, relid, pretty)
}

/// The relation a deparser's first argument names: an OID, or a name as text, qualified or found
/// on the search path (pg_catalog, then public).
fn view_oid(catalog: &Catalog, v: &Value) -> Result<Option<u32>> {
    if let Some(oid) = oid_of(v) {
        return Ok(Some(oid));
    }
    let Some(text) = v.text() else {
        return Ok(None);
    };
    let parts: Vec<String> = text
        .split('.')
        .map(|p| match p.strip_prefix('"').and_then(|x| x.strip_suffix('"')) {
            Some(q) => q.replace("\"\"", "\""),
            None => p.to_lowercase(),
        })
        .collect();
    let rows = crate::pgcatalog::built_in("pg_class", catalog.release);
    let built_in = |schema: u32, name: &str| {
        rows.iter()
            .find(|r| {
                r.get("relname").cloned().flatten().as_deref() == Some(name)
                    && r.get("relnamespace").cloned().flatten().and_then(|s| s.parse::<u32>().ok()) == Some(schema)
            })
            .and_then(|r| r.get("oid").cloned().flatten()?.parse::<u32>().ok())
    };
    let found = match parts.as_slice() {
        [n] => built_in(11, n).or_else(|| catalog.oids.of(&Object::Class(full(n)))),
        [s, n] => match s.as_str() {
            "pg_catalog" => built_in(11, n),
            "information_schema" => {
                let ns = crate::regs::reg_parse(crate::regs::RegKind::Namespace, s, catalog.release).unwrap_or(0);
                built_in(ns, n)
            }
            _ => catalog.oids.of(&Object::Class(format!("{s}.{n}"))),
        },
        _ => None,
    };
    found
        .map(Some)
        .ok_or_else(|| Error::new(format!("relation \"{text}\" does not exist")))
}
