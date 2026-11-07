//! PostgreSQL's own views of pg_catalog and information_schema, each read as a step of the question
//! that names it, its definition the file's release's, as that release's server writes it
//! (`pgviews.txt`). A snail has no roles and no server around it, so in each definition a privilege
//! test holds, no schema is another session's temporary one, and a column computed from what only a
//! server fills (an owner's name, the database's name, a size) is NULL and refused by name where a
//! statement asks it; information_schema's domains are read as their base types and its own
//! functions as the expressions they return. The server's statistics views are refused whole.

use crate::{Error, Result};
use sqlparser::ast::{
    DataType, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, ObjectNamePart, Query,
    SelectItem, SetExpr, Value,
};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::sync::OnceLock;

/// Each carried definition by its key, `view schema.name` or `function schema.name(args)`: the
/// releases it holds for and its text as COPY writes it.
fn carried() -> &'static HashMap<&'static str, Vec<(u8, &'static str)>> {
    static CARRIED: OnceLock<HashMap<&'static str, Vec<(u8, &'static str)>>> = OnceLock::new();
    CARRIED.get_or_init(|| {
        let mut all: HashMap<&'static str, Vec<(u8, &'static str)>> = HashMap::new();
        let mut at = "";
        for line in include_str!("../pgviews.txt").lines() {
            if let Some(k) = line.strip_prefix("= ") {
                at = k;
                continue;
            }
            let (spec, text) = line
                .split_once('\t')
                .expect("a carried definition names its releases");
            all.entry(at)
                .or_default()
                .push((crate::regs::releases_of(Some(spec)), text));
        }
        all
    })
}

/// A carried definition under a release, its text read back from COPY's.
fn definition(key: &str, release: u8) -> Option<String> {
    let bit = 1u8 << (release - crate::regs::RELEASES.start());
    carried()
        .get(key)?
        .iter()
        .find(|(r, _)| r & bit != 0)
        .and_then(|(_, t)| crate::pgcatalog::unescaped_text(t))
}

/// The view a relation's name names, `schema.name`: pg_catalog's searched first for a name alone;
/// none for any other name.
pub(crate) fn view_named(name: &str) -> Option<String> {
    let full = match name.split_once('.') {
        Some(("pg_catalog" | "information_schema", _)) => name.to_string(),
        Some(_) => return None,
        None => format!("pg_catalog.{name}"),
    };
    carried()
        .contains_key(format!("view {full}").as_str())
        .then_some(full)
}

thread_local! {
    /// The columns of each view lifted to a step that a statement may not ask, by the step's name.
    static REFUSED: RefCell<HashMap<String, Vec<Option<String>>>> = RefCell::new(HashMap::new());
}

/// What a statement's views refuse, held while it runs.
pub(crate) struct Holding;

pub(crate) fn holding() -> Holding {
    REFUSED.with(|r| r.borrow_mut().clear());
    Holding
}

impl Drop for Holding {
    fn drop(&mut self) {
        REFUSED.with(|r| r.borrow_mut().clear());
    }
}

/// The columns of a step a view was lifted to that a statement may not ask.
pub(crate) fn refused_of(step: &str) -> Vec<Option<String>> {
    REFUSED.with(|r| r.borrow().get(step).cloned().unwrap_or_default())
}

pub(crate) fn note_refused(step: &str, refused: Vec<Option<String>>) {
    REFUSED.with(|r| r.borrow_mut().insert(step.to_string(), refused));
}

/// The definitions a statement's views read, those of the views they read in turn among them: what
/// the catalog tables it makes first must cover.
pub(crate) fn texts_read(words: &[String], release: u8) -> Vec<String> {
    // Each carried definition a word names: a view's, or one of information_schema's functions'.
    let named = |w: &str| -> Option<String> {
        if let Some(v) = view_named(w) {
            return Some(format!("view {v}"));
        }
        let f = w.strip_prefix("information_schema.")?;
        carried()
            .keys()
            .find(|k| {
                k.strip_prefix("function information_schema.")
                    .is_some_and(|g| g.split('(').next() == Some(f))
            })
            .map(|k| k.to_string())
    };
    let mut seen: Vec<String> = Vec::new();
    let mut todo: Vec<String> = words.iter().filter_map(|w| named(w)).collect();
    let mut out = Vec::new();
    while let Some(k) = todo.pop() {
        if seen.contains(&k) {
            continue;
        }
        seen.push(k.clone());
        if let Some(d) = definition(&k, release) {
            todo.extend(words_of(&d).iter().filter_map(|w| named(w)));
            out.push(d);
        }
    }
    out
}

/// A text's words, as the statement's tables are named: an unquoted word folded.
pub(crate) fn words_of(sql: &str) -> Vec<String> {
    let Ok(tokens) = crate::read::image::tokens(sql) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        if let sqlparser::tokenizer::Token::Word(w) = &tokens[i].token {
            let mut v = folded(w);
            // A qualified name read whole: `schema.name`.
            if matches!(
                tokens.get(i + 1).map(|t| &t.token),
                Some(sqlparser::tokenizer::Token::Period)
            ) {
                if let Some(sqlparser::tokenizer::Token::Word(n)) =
                    tokens.get(i + 2).map(|t| &t.token)
                {
                    out.push(v.clone());
                    v = format!("{v}.{}", folded(n));
                    i += 2;
                }
            }
            out.push(v);
        }
        i += 1;
    }
    out
}

fn folded(w: &sqlparser::tokenizer::Word) -> String {
    if w.quote_style.is_some() {
        w.value.clone()
    } else {
        w.value.to_lowercase()
    }
}

/// The views whose rows only a server holds, by what they hold: each refused whole.
const SERVERS_VIEWS: &[(&[&str], &str)] = &[
    (
        &[
            "pg_roles",
            "pg_user",
            "pg_shadow",
            "pg_group",
            "administrable_role_authorizations",
            "applicable_roles",
            "enabled_roles",
            "role_column_grants",
            "role_routine_grants",
            "role_table_grants",
            "role_udt_grants",
            "role_usage_grants",
        ],
        "roles",
    ),
    (
        &[
            "column_privileges",
            "routine_privileges",
            "table_privileges",
            "udt_privileges",
            "usage_privileges",
        ],
        "privileges granted to roles",
    ),
    (
        &[
            "pg_user_mappings",
            "user_mappings",
            "user_mapping_options",
            "_pg_user_mappings",
        ],
        "user mappings, a role's",
    ),
    (
        &["pg_cursors", "pg_prepared_statements"],
        "a session's cursors and prepared statements",
    ),
    (&["pg_locks", "pg_prepared_xacts"], "transactions' locks"),
    (
        &[
            "pg_settings",
            "pg_file_settings",
            "pg_hba_file_rules",
            "pg_ident_file_mappings",
            "pg_config",
        ],
        "its settings and its files",
    ),
    (
        &[
            "pg_shmem_allocations",
            "pg_shmem_allocations_numa",
            "pg_backend_memory_contexts",
            "pg_dsm_registry_allocations",
            "pg_aios",
            "pg_wait_events",
        ],
        "its memory and its processes",
    ),
    (
        &["pg_available_extensions", "pg_available_extension_versions"],
        "the extensions installed beside it",
    ),
    (
        &[
            "pg_replication_slots",
            "pg_replication_origin_status",
            "pg_publication_tables",
            "pg_publication_sequences",
        ],
        "replication",
    ),
    (
        &["pg_timezone_names", "pg_timezone_abbrevs"],
        "the time zones installed beside it",
    ),
];

/// The functions whose value only a server knows: a column computed from one is refused.
const SERVERS_FUNCTIONS: &[&str] = &[
    "pg_get_userbyid",
    "current_database",
    "current_user",
    "session_user",
    "current_role",
    "user",
    "pg_relation_size",
    "pg_total_relation_size",
    "pg_table_size",
    "pg_indexes_size",
    "pg_relation_filenode",
    "pg_relation_filepath",
    "pg_tablespace_location",
    "pg_postmaster_start_time",
    "pg_backend_pid",
    "inet_server_addr",
    "inet_server_port",
];

/// The columns whose value only a server fills, by their names.
fn servers_column(name: &str) -> bool {
    name.ends_with("owner")
        || name.ends_with("acl")
        || name.starts_with("rol")
        || name == "datname"
        || matches!(
            name,
            "relpages"
                | "reltuples"
                | "relallvisible"
                | "relallfrozen"
                | "relfrozenxid"
                | "relminmxid"
                | "relfilenode"
                | "attmissingval"
                | "initprivs"
                | "datdba"
        )
}

/// The privilege tests, which hold for whoever opens a file: it has no roles.
fn privilege_test(name: &str) -> bool {
    name == "pg_has_role" || (name.starts_with("has_") && name.ends_with("_privilege"))
}

fn function_name(f: &sqlparser::ast::Function) -> String {
    let n = crate::catalog::object_name(&f.name).to_lowercase();
    n.strip_prefix("pg_catalog.").unwrap_or(&n).to_string()
}

/// What only a server could fill that an expression reads, where it reads one.
fn servers_read(e: &Expr) -> Option<String> {
    let mut found = None;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        let why = match x {
            Expr::Function(f) if SERVERS_FUNCTIONS.contains(&function_name(f).as_str()) => {
                Some(format!("{}() is a server's own", function_name(f)))
            }
            Expr::Identifier(i) if servers_column(&i.value) => {
                Some(format!("{} is a server's own", i.value))
            }
            Expr::CompoundIdentifier(p) if p.last().is_some_and(|i| servers_column(&i.value)) => {
                Some(format!("{} is a server's own", p.last().unwrap().value))
            }
            _ => None,
        };
        if found.is_none() {
            found = why;
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// A view as a step reads it, under a release: its definition rewritten, and each of its columns'
/// refusal. None for a name that is none of PostgreSQL's views.
pub(crate) fn system_view(release: u8, name: &str) -> Result<Option<(Query, Vec<Option<String>>)>> {
    let Some(full) = view_named(name) else {
        return Ok(None);
    };
    let bare = full.rsplit('.').next().unwrap_or(&full);
    let Some(text) = definition(&format!("view {full}"), release) else {
        return Err(Error::new(format!("relation \"{name}\" does not exist")));
    };
    let holds = match bare.starts_with("pg_stat") {
        true => Some("a server's statistics"),
        false => SERVERS_VIEWS
            .iter()
            .find(|(names, _)| names.contains(&bare))
            .map(|(_, holds)| *holds),
    };
    if let Some(holds) = holds {
        return Err(Error::new(format!(
            "{full} holds {holds}, a running server's own, and a snail has no server: it is not \
             read"
        )));
    }
    let parsed = crate::read::image::parse_sql(&super::spelled::spelled(&text)?)?;
    let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
        return Err(Error::new(format!("{full}'s definition is no question")));
    };
    let mut q = (**q).clone();
    let refused = rewrite(&mut q, release)?;
    Ok(Some((super::spelled::between_written(&q), refused)))
}

/// Each output column's refusal: a select item reading what only a server fills is NULL. Then,
/// throughout, the privilege tests and the temporary schemas answered, the domains read as their
/// base types, information_schema's functions inlined.
fn rewrite(q: &mut Query, release: u8) -> Result<Vec<Option<String>>> {
    let mut refused: Vec<Option<String>> = Vec::new();
    fn selects<'a>(body: &'a mut SetExpr, out: &mut Vec<&'a mut sqlparser::ast::Select>) {
        match body {
            SetExpr::Select(s) => out.push(s),
            SetExpr::SetOperation { left, right, .. } => {
                selects(left, out);
                selects(right, out);
            }
            SetExpr::Query(q) => selects(&mut q.body, out),
            _ => {}
        }
    }
    let mut arms = Vec::new();
    selects(&mut q.body, &mut arms);
    for s in arms {
        for (i, item) in s.projection.iter_mut().enumerate() {
            let e = match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => continue,
            };
            if let Some(why) = servers_read(e) {
                if refused.len() <= i {
                    refused.resize(i + 1, None);
                }
                refused[i].get_or_insert(why);
                *e = Expr::Value(Value::Null.into());
                if let SelectItem::UnnamedExpr(_) = item {
                    // An unnamed item keeps no name once it is NULL: PostgreSQL names it by its
                    // function, which the view writes with an alias wherever one is read.
                }
            }
        }
    }
    let mut failed: Option<Error> = None;
    let _ = sqlparser::ast::visit_expressions_mut(q, |e| {
        if let Err(err) = answer(e, release) {
            failed.get_or_insert(err);
        }
        ControlFlow::<()>::Continue(())
    });
    if let Some(e) = failed {
        return Err(e);
    }
    let _ = sqlparser::ast::visit_expressions_mut(q, |e| {
        if let Expr::Cast { data_type, .. } = e {
            if let Some(base) = domain_base(data_type) {
                *data_type = base;
            }
        }
        ControlFlow::<()>::Continue(())
    });
    let _ = sqlparser::ast::VisitMut::visit(q, &mut Commas);
    Ok(refused)
}

/// Each comma of a view's FROM met by the conjuncts of its WHERE that tie the item after it to
/// those before it, as the ON of a join in the FROM's order, the rest left in the WHERE.
struct Commas;

impl sqlparser::ast::VisitorMut for Commas {
    type Break = ();

    fn post_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        tie_commas(&mut q.body);
        ControlFlow::Continue(())
    }
}

fn tie_commas(body: &mut SetExpr) {
    match body {
        SetExpr::Select(s) => tie_select(s),
        SetExpr::SetOperation { left, right, .. } => {
            tie_commas(left);
            tie_commas(right);
        }
        _ => {}
    }
}

fn tie_select(s: &mut sqlparser::ast::Select) {
    use sqlparser::ast::{Join, JoinConstraint, JoinOperator, TableFactor};
    if s.from.len() < 2 || s.selection.is_none() {
        return;
    }
    let mut conjuncts = Vec::new();
    split_and(s.selection.take().unwrap(), &mut conjuncts);
    let mut items = std::mem::take(&mut s.from).into_iter();
    let mut first = items.next().unwrap();
    let mut before = Vec::new();
    item_qualifiers(&first, &mut before);
    for item in items {
        let mut mine = Vec::new();
        item_qualifiers(&item, &mut mine);
        let mut on = Vec::new();
        let mut i = 0;
        while i < conjuncts.len() {
            let placed = qualifiers_read(&conjuncts[i]).is_some_and(|qs| {
                qs.iter().any(|q| mine.contains(q))
                    && qs.iter().all(|q| mine.contains(q) || before.contains(q))
            });
            if placed {
                on.push(conjuncts.remove(i));
            } else {
                i += 1;
            }
        }
        before.extend(mine);
        let relation = match item.joins.is_empty() {
            true => item.relation,
            false => TableFactor::NestedJoin {
                table_with_joins: Box::new(item),
                alias: None,
            },
        };
        let join_operator = match and_all(on) {
            Some(e) => JoinOperator::Join(JoinConstraint::On(e)),
            None => JoinOperator::CrossJoin(JoinConstraint::None),
        };
        first.joins.push(Join {
            relation,
            global: false,
            join_operator,
        });
    }
    s.from = vec![first];
    s.selection = and_all(conjuncts);
}

fn split_and(e: Expr, out: &mut Vec<Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: sqlparser::ast::BinaryOperator::And,
            right,
        } => {
            split_and(*left, out);
            split_and(*right, out);
        }
        Expr::Nested(inner)
            if matches!(
                inner.as_ref(),
                Expr::BinaryOp {
                    op: sqlparser::ast::BinaryOperator::And,
                    ..
                }
            ) =>
        {
            split_and(*inner, out)
        }
        other => out.push(other),
    }
}

fn and_all(es: Vec<Expr>) -> Option<Expr> {
    es.into_iter().reduce(|a, b| Expr::BinaryOp {
        left: Box::new(a),
        op: sqlparser::ast::BinaryOperator::And,
        right: Box::new(b),
    })
}

/// The qualifiers an item of a FROM brings, its joins' among them.
fn item_qualifiers(item: &sqlparser::ast::TableWithJoins, out: &mut Vec<String>) {
    factor_qualifiers(&item.relation, out);
    for j in &item.joins {
        factor_qualifiers(&j.relation, out);
    }
}

fn factor_qualifiers(t: &sqlparser::ast::TableFactor, out: &mut Vec<String>) {
    use sqlparser::ast::TableFactor;
    let alias = match t {
        TableFactor::Table { name, alias, .. } => match alias {
            Some(a) => Some(&a.name),
            None => {
                if let Some(ObjectNamePart::Identifier(i)) = name.0.last() {
                    out.push(ident_folded(i));
                }
                return;
            }
        },
        TableFactor::NestedJoin {
            table_with_joins,
            alias: None,
        } => return item_qualifiers(table_with_joins, out),
        TableFactor::NestedJoin { alias, .. }
        | TableFactor::Derived { alias, .. }
        | TableFactor::Function { alias, .. }
        | TableFactor::TableFunction { alias, .. }
        | TableFactor::UNNEST { alias, .. } => alias.as_ref().map(|a| &a.name),
        _ => None,
    };
    if let Some(a) = alias {
        out.push(ident_folded(a));
    }
}

fn ident_folded(i: &sqlparser::ast::Ident) -> String {
    match i.quote_style {
        Some(_) => i.value.clone(),
        None => i.value.to_lowercase(),
    }
}

/// The qualifiers a conjunct's columns name, where each is named by one and it holds no
/// subquery; none otherwise.
fn qualifiers_read(e: &Expr) -> Option<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    let mut whole = true;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        match x {
            Expr::Identifier(_)
            | Expr::Subquery(_)
            | Expr::Exists { .. }
            | Expr::InSubquery { .. } => whole = false,
            Expr::CompoundIdentifier(p) if p.len() >= 2 => {
                let q = ident_folded(&p[p.len() - 2]);
                if !out.contains(&q) {
                    out.push(q);
                }
            }
            _ => {}
        }
        ControlFlow::<()>::Continue(())
    });
    whole.then_some(out)
}

/// One expression answered: a privilege test true, another session's temporary schema none, one of
/// information_schema's functions as the expression it returns.
fn answer(e: &mut Expr, release: u8) -> Result<()> {
    let Expr::Function(f) = e else {
        return Ok(());
    };
    let name = function_name(f);
    if privilege_test(&name) {
        *e = Expr::Value(Value::Boolean(true).into());
        return Ok(());
    }
    match name.as_str() {
        "pg_is_other_temp_schema" => {
            *e = Expr::Value(Value::Boolean(false).into());
            return Ok(());
        }
        "pg_my_temp_schema" => {
            *e = expression("CAST(0 AS oid)")?;
            return Ok(());
        }
        // The file is the database a view describes, under the name its pg_database row holds.
        "current_database" => {
            *e = expression(&format!("CAST('{}' AS name)", crate::pgcatalog::DATABASE))?;
            return Ok(());
        }
        "getdatabaseencoding" => {
            *e = expression("CAST('UTF8' AS name)")?;
            return Ok(());
        }
        // What a snail's own writes take: a table's rows, never a view's or another relation's.
        "pg_relation_is_updatable" | "pg_column_is_updatable" => {
            let args = arguments(f);
            let Some(rel) = args.first() else {
                return Ok(());
            };
            let what = match (name.as_str(), args.get(1)) {
                ("pg_relation_is_updatable", _) => {
                    "CASE WHEN snail_rel.relkind IN ('r', 'p') THEN 28 ELSE 0 END".to_string()
                }
                (_, Some(attnum)) => format!("snail_rel.relkind IN ('r', 'p') AND ({attnum}) > 0"),
                _ => return Ok(()),
            };
            *e = expression(&format!(
                "(SELECT {what} FROM pg_catalog.pg_class snail_rel WHERE snail_rel.oid = \
                 CAST(({rel}) AS oid))"
            ))?;
            return Ok(());
        }
        _ => {}
    }
    if let Some(own) = name.strip_prefix("information_schema.") {
        if own == "_pg_expandarray" {
            return Ok(());
        }
        let args = arguments(f);
        let mut inlined = inline(own, &args, release)?;
        let mut failed: Option<Error> = None;
        let _ = sqlparser::ast::visit_expressions_mut(&mut inlined, |x| {
            if let Err(err) = answer(x, release) {
                failed.get_or_insert(err);
            }
            ControlFlow::<()>::Continue(())
        });
        if let Some(err) = failed {
            return Err(err);
        }
        *e = Expr::Nested(Box::new(inlined));
    }
    Ok(())
}

/// A function call's arguments, each as an expression: a row argument `a.*` as its relation's name.
fn arguments(f: &sqlparser::ast::Function) -> Vec<Expr> {
    let FunctionArguments::List(l) = &f.args else {
        return Vec::new();
    };
    l.args
        .iter()
        .filter_map(|a| match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e.clone()),
            FunctionArg::Unnamed(FunctionArgExpr::QualifiedWildcard(n)) => Some(Expr::Identifier(
                sqlparser::ast::Ident::new(crate::catalog::object_name(n)),
            )),
            _ => None,
        })
        .collect()
}

/// One of information_schema's functions as the expression it returns, its arguments in place of
/// its parameters: `($2).typtype` the second argument's relation's column.
fn inline(name: &str, args: &[Expr], release: u8) -> Result<Expr> {
    let key = carried()
        .keys()
        .find(|k| {
            k.strip_prefix("function information_schema.")
                .is_some_and(|f| f.split('(').next() == Some(name))
        })
        .ok_or_else(|| Error::new(format!("information_schema.{name} is not read")))?;
    let text = definition(key, release)
        .ok_or_else(|| Error::new(format!("information_schema.{name} is not in this release")))?;
    let body = text
        .split_once("\nRETURN ")
        .map(|(_, b)| b.trim())
        .ok_or_else(|| Error::new(format!("information_schema.{name}'s body is no expression")))?;
    let params: Vec<String> = key
        .split_once('(')
        .map(|(_, p)| p.trim_end_matches(')'))
        .unwrap_or("")
        .split(", ")
        .map(|p| p.split(' ').next().unwrap_or("").to_string())
        .collect();
    let mut e = expression(&super::spelled::spelled(body)?)?;
    let arg_of = |n: &str| -> Option<Expr> {
        if let Some(i) = n.strip_prefix('$').and_then(|d| d.parse::<usize>().ok()) {
            return args.get(i - 1).cloned();
        }
        params
            .iter()
            .position(|p| p == n)
            .and_then(|i| args.get(i).cloned())
    };
    // A field of a parameter first, `($2).typtype` its relation's column, before its parameter alone
    // is replaced: the visit reaches a parameter before the field access around it.
    for fields in [true, false] {
    let _ = sqlparser::ast::visit_expressions_mut(&mut e, |x| {
        let replaced = match x {
            Expr::Identifier(i) if i.quote_style.is_none() && !fields => arg_of(&i.value),
            Expr::Value(v) if !fields => match &v.value {
                Value::Placeholder(p) => arg_of(p),
                _ => None,
            },
            Expr::CompoundFieldAccess { root, access_chain } if fields => {
                let mut r = root.as_ref();
                while let Expr::Nested(inner) = r {
                    r = inner;
                }
                let base = match r {
                    Expr::Value(v) => match &v.value {
                        Value::Placeholder(p) => arg_of(p),
                        _ => None,
                    },
                    Expr::Identifier(i) => arg_of(&i.value),
                    _ => None,
                };
                match (base, access_chain.as_slice()) {
                    (
                        Some(Expr::Identifier(rel)),
                        [sqlparser::ast::AccessExpr::Dot(Expr::Identifier(field))],
                    ) => Some(Expr::CompoundIdentifier(vec![rel, field.clone()])),
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(r) = replaced {
            *x = Expr::Nested(Box::new(r));
        }
        ControlFlow::<()>::Continue(())
    });
    }
    Ok(e)
}

/// information_schema's domains, each as its base type.
fn domain_base(t: &DataType) -> Option<DataType> {
    let DataType::Custom(n, _) = t else {
        return None;
    };
    let parts: Vec<String> =
        n.0.iter()
            .map(|p| match p {
                ObjectNamePart::Identifier(i) => i.value.to_lowercase(),
                other => other.to_string(),
            })
            .collect();
    let base = match parts.as_slice() {
        [s, d] if s == "information_schema" => match d.as_str() {
            "sql_identifier" => "name",
            "cardinal_number" => "integer",
            "character_data" => "character varying",
            "yes_or_no" => "character varying(3)",
            "time_stamp" => "timestamp(2) with time zone",
            _ => return None,
        },
        _ => return None,
    };
    crate::ddl::parse_type(base).ok()
}

/// An expression read from its text.
fn expression(s: &str) -> Result<Expr> {
    super::image::parse_expr(s).map_err(|e| Error::new(format!("{s}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_of_a_row_argument_is_its_relations_column() {
        let args = [Expr::Identifier("a".into()), Expr::Identifier("t".into())];
        let e = inline("_pg_truetypid", &args, 18).unwrap().to_string();
        assert!(e.contains("t.typtype") && e.contains("t.typbasetype") && e.contains("a.atttypid"), "{e}");
    }

    fn tied(sql: &str) -> String {
        let parsed = crate::read::image::parse_sql(sql).unwrap();
        let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
            panic!("a question")
        };
        let mut q = (**q).clone();
        let _ = sqlparser::ast::VisitMut::visit(&mut q, &mut Commas);
        q.to_string()
    }

    #[test]
    fn a_comma_is_met_by_the_conjuncts_that_tie_its_item_to_those_before_it() {
        assert_eq!(
            tied("SELECT a.x FROM a, b, c WHERE a.x = b.y AND c.z = 1 AND b.w = c.w AND a.v > 0"),
            "SELECT a.x FROM a JOIN b ON a.x = b.y JOIN c ON c.z = 1 AND b.w = c.w WHERE a.v > 0"
        );
        // A conjunct naming a column by no qualifier, or holding a subquery, stays in the WHERE;
        // an item holding joins of its own is met whole.
        assert_eq!(
            tied(
                "SELECT 1 FROM a, b LEFT JOIN d ON d.k = b.k WHERE x = b.y AND a.x = d.k \
                  AND a.y = (SELECT 1)"
            ),
            "SELECT 1 FROM a JOIN (b LEFT JOIN d ON d.k = b.k) ON a.x = d.k WHERE x = b.y AND \
             a.y = (SELECT 1)"
        );
    }
}
