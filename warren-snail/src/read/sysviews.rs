//! PostgreSQL's own views of pg_catalog and information_schema, each read as a step of the question
//! that names it, its definition the file's release's, as that release's server writes it
//! (`pgviews.txt`). A snail has no roles and no server around it, so in each definition a privilege
//! test holds, no schema is another session's temporary one, and a column computed from what only a
//! server fills (an owner's name, the database's name, a size) is NULL and refused by name where a
//! statement asks it; information_schema's domains are read as their base types and its own
//! functions as the expressions they return. The server's statistics views are refused whole.
//! pg_prepared_statements reads the session's prepared statements, but for a plan cache's counts.

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

/// A built-in view's text as pg_get_viewdef writes it, pretty or plain, under a release.
pub(crate) fn carried_viewdef(full: &str, release: u8, pretty: bool) -> Option<String> {
    let key = if pretty { format!("view {full}") } else { format!("plain {full}") };
    definition(&key, release)
}

/// A carried text by its key, under a release: a built-in function's arguments with their
/// defaults (`fnargs oid`), one default (`fndefault oid n`), its SQL body (`fnbody oid`) or its
/// definition (`fndef oid`).
pub(crate) fn carried_definition(key: &str, release: u8) -> Option<String> {
    definition(key, release)
}

/// A built-in rule's text other than a view's, `schema.relation.rule`, as pg_get_ruledef writes it.
pub(crate) fn carried_ruledef(full: &str, release: u8, pretty: bool) -> Option<String> {
    let key = if pretty { format!("rule {full}") } else { format!("plain rule {full}") };
    definition(&key, release)
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
    /// How many questions are under way on this thread, a subquery's inside its statement's.
    static HELD: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// What a statement's views refuse, held while it runs, a subquery's beside its statement's.
pub(crate) struct Holding {
    outermost: bool,
}

pub(crate) fn holding() -> Holding {
    let outermost = HELD.with(|h| h.replace(h.get() + 1)) == 0;
    if outermost {
        REFUSED.with(|r| r.borrow_mut().clear());
    }
    Holding { outermost }
}

impl Holding {
    /// Whether it holds the statement's own question, no subquery's.
    pub(crate) fn outermost(&self) -> bool {
        self.outermost
    }
}

impl Drop for Holding {
    fn drop(&mut self) {
        HELD.with(|h| h.set(h.get().saturating_sub(1)));
        if self.outermost {
            REFUSED.with(|r| r.borrow_mut().clear());
        }
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
    (&["pg_cursors"], "a session's cursors"),
    (&["pg_locks", "pg_prepared_xacts"], "transactions' locks"),
    (
        &[
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
            Expr::Identifier(i) if super::prepared::PLAN_CACHE.contains(&i.value.as_str()) => {
                Some(super::prepared::plan_cache_refusal(&i.value))
            }
            Expr::CompoundIdentifier(p)
                if p.last()
                    .is_some_and(|i| super::prepared::PLAN_CACHE.contains(&i.value.as_str())) =>
            {
                Some(super::prepared::plan_cache_refusal(&p.last().unwrap().value))
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

/// The columns refused of the views of PostgreSQL's that a SELECT's FROM reads, each by the name
/// the FROM gives the view and the column's own, with its refusal.
fn refused_beneath(
    catalog: &crate::catalog::Catalog,
    s: &sqlparser::ast::Select,
) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    let factors = s
        .from
        .iter()
        .flat_map(|t| std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)));
    for f in factors {
        // A view is named alone: a call, `pg_timezone_names()` in its own view, is a function's.
        let sqlparser::ast::TableFactor::Table {
            name,
            alias,
            args: None,
            ..
        } = f
        else {
            continue;
        };
        let n = crate::catalog::object_name(name);
        // A view refused whole is refused where it is lifted.
        let Ok(Some((q, refused))) = system_view(catalog, &n) else {
            continue;
        };
        let SetExpr::Select(inner) = q.body.as_ref() else {
            continue;
        };
        let called = match alias {
            Some(a) => a.name.value.clone(),
            None => n.rsplit('.').next().unwrap_or(&n).to_string(),
        };
        for (item, why) in inner.projection.iter().zip(&refused) {
            let (Some(why), Some(column)) = (why, output_name(item)) else {
                continue;
            };
            out.push((called.clone(), column, why.clone()));
        }
    }
    out
}

/// The name a select item gives its column, where it gives one.
fn output_name(item: &SelectItem) -> Option<String> {
    match item {
        SelectItem::ExprWithAlias { alias, .. } => Some(alias.value.clone()),
        SelectItem::UnnamedExpr(Expr::Identifier(i)) => Some(i.value.clone()),
        SelectItem::UnnamedExpr(Expr::CompoundIdentifier(p)) => p.last().map(|i| i.value.clone()),
        _ => None,
    }
}

/// A refused column of a view beneath that an expression reads, where it reads one: the select
/// item passing it through is refused with it.
fn read_beneath(e: &Expr, beneath: &[(String, String, String)]) -> Option<String> {
    let mut found = None;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        let read = match x {
            Expr::Identifier(c) => Some((None, &c.value)),
            Expr::CompoundIdentifier(p) if p.len() == 2 => Some((Some(&p[0].value), &p[1].value)),
            _ => None,
        };
        if let Some((q, c)) = read {
            let hit = beneath
                .iter()
                .find(|(v, col, _)| col == c && q.is_none_or(|q| q == v));
            if let (None, Some((_, _, why))) = (&found, hit) {
                found = Some(why.clone());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    found
}

/// A view as a step reads it, under a release: its definition rewritten, and each of its columns'
/// refusal. None for a name that is none of PostgreSQL's views.
pub(crate) fn system_view(
    catalog: &crate::catalog::Catalog,
    name: &str,
) -> Result<Option<(Query, Vec<Option<String>>)>> {
    let release = catalog.release;
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
    if bare == "pg_settings" {
        settings_read(&mut q)?;
    }
    let refused = rewrite(&mut q, catalog)?;
    Ok(Some((super::spelled::between_written(&q), refused)))
}

/// pg_settings read from the session's rows of every setting, where its definition reads
/// pg_show_all_settings: each value a server's own setting holds is NULL there, and refused where
/// a statement reads it (`settings_values`).
fn settings_read(q: &mut Query) -> Result<()> {
    let SetExpr::Select(s) = q.body.as_mut() else {
        return Err(Error::new("pg_settings' definition is no question"));
    };
    for t in &mut s.from {
        if let sqlparser::ast::TableFactor::Table { name, args: Some(_), .. } = &mut t.relation {
            if crate::catalog::object_name(name) == "pg_show_all_settings" {
                *name = sqlparser::ast::ObjectName::from(vec![sqlparser::ast::Ident::with_quote(
                    '"',
                    super::settings::ROWS,
                )]);
            }
        }
    }
    Ok(())
}

/// A SELECT whose FROM reads pg_settings, its expressions that read a value a setting holds
/// (`settings::VALUES`) reading it through the refusal of a server's own, by the row's name: an
/// item that is the value alone keeps its column's name, a star over pg_settings is its columns
/// so read, and its whole row is refused, as a server's own values in it are. A bare name is
/// pg_settings' where it is the FROM's one pg_settings, and in a subquery only where no FROM
/// stands between them. A WITH query of the name is no view.
pub(crate) fn settings_values(q: &mut Query, visible: &[String]) -> Result<()> {
    use sqlparser::ast::{JoinConstraint, JoinOperator, TableFactor, TableWithJoins, VisitMut};
    let SetExpr::Select(s) = q.body.as_mut() else {
        return Ok(());
    };
    // Each FROM relation in order: the name it goes by, and whether it is pg_settings.
    fn relations(t: &TableWithJoins, visible: &[String], out: &mut Vec<(Option<String>, bool)>) {
        for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            out.push(match f {
                TableFactor::Table {
                    name, alias, args, ..
                } => {
                    let n = crate::catalog::object_name(name);
                    let settings = args.is_none()
                        && !visible.contains(&n)
                        && view_named(&n).as_deref() == Some("pg_catalog.pg_settings");
                    let by = match alias {
                        Some(a) => crate::catalog::ident(&a.name),
                        None => n.rsplit('.').next().unwrap_or(&n).to_string(),
                    };
                    (Some(by), settings)
                }
                TableFactor::Derived { alias, .. } => {
                    (alias.as_ref().map(|a| crate::catalog::ident(&a.name)), false)
                }
                TableFactor::NestedJoin {
                    table_with_joins, ..
                } => {
                    relations(table_with_joins, visible, out);
                    continue;
                }
                _ => (None, false),
            });
        }
    }
    let mut from = Vec::new();
    for t in &s.from {
        relations(t, visible, &mut from);
    }
    let settings: Vec<&str> = from
        .iter()
        .filter(|(_, s)| *s)
        .filter_map(|(n, _)| n.as_deref())
        .collect();
    let [relation] = settings.as_slice() else {
        return Ok(());
    };
    let relation = relation.to_string();
    // A join USING or NATURAL merges columns a star gives once.
    let merged = s.from.iter().flat_map(|t| &t.joins).any(|j| {
        let on = match &j.join_operator {
            JoinOperator::Join(c)
            | JoinOperator::Inner(c)
            | JoinOperator::Left(c)
            | JoinOperator::LeftOuter(c)
            | JoinOperator::Right(c)
            | JoinOperator::RightOuter(c)
            | JoinOperator::FullOuter(c) => Some(c),
            _ => None,
        };
        matches!(on, Some(JoinConstraint::Using(_) | JoinConstraint::Natural))
    });
    // pg_settings' columns as a star gives them, each value read through its refusal.
    let columns = |by: &str| -> Result<Vec<SelectItem>> {
        let q = super::relations::quote(by);
        let list: Vec<String> = super::settings::columns()
            .iter()
            .map(|(c, _)| format!("{q}.{c}"))
            .collect();
        let mut items = select_list(&list.join(", "))?;
        for item in &mut items {
            if let SelectItem::UnnamedExpr(e) = item {
                if let Some(c) = value_column(e, by) {
                    *item = SelectItem::ExprWithAlias {
                        expr: value_read(e, by, &c)?,
                        alias: sqlparser::ast::Ident::new(c),
                    };
                }
            }
        }
        Ok(items)
    };
    let mut projection = Vec::with_capacity(s.projection.len());
    // A star over joins USING or NATURAL: each join's merged columns once, first, then each side's
    // others, as PostgreSQL's parser writes the star out.
    let joined = |t: &TableWithJoins| -> Result<Vec<(String, SelectItem)>> {
        let leaf = |f: &TableFactor| -> Result<Vec<(String, SelectItem)>> {
            let unknown = || {
                Error::new(
                    "* is not read beside pg_settings here: a server's own setting's values are \
                     refused where they are read, so name each column",
                )
            };
            let (by, names) = match f {
                TableFactor::Table { name, alias, .. } => {
                    let n = crate::catalog::object_name(name);
                    let by = match alias {
                        Some(a) => crate::catalog::ident(&a.name),
                        None => n.rsplit('.').next().unwrap_or(&n).to_string(),
                    };
                    if by != relation {
                        return Err(unknown());
                    }
                    let items = columns(&by)?;
                    let names = super::settings::columns().map(|(c, _)| c.to_string());
                    return Ok(names.into_iter().zip(items).collect());
                }
                TableFactor::Derived {
                    subquery,
                    alias: Some(a),
                    ..
                } => {
                    let renames: Vec<String> =
                        a.columns.iter().map(|c| crate::catalog::ident(&c.name)).collect();
                    let names = super::outputs::names_of(subquery, &renames).ok_or_else(unknown)?;
                    (crate::catalog::ident(&a.name), names)
                }
                _ => return Err(unknown()),
            };
            let q = super::relations::quote(&by);
            names
                .into_iter()
                .map(|c| {
                    let item = select_list(&format!("{q}.{}", super::relations::quote(&c)))?;
                    Ok((c, item.into_iter().next().ok_or_else(unknown)?))
                })
                .collect()
        };
        let mut out = leaf(&t.relation)?;
        for j in &t.joins {
            let right = leaf(&j.relation)?;
            let (constraint, side) = match &j.join_operator {
                JoinOperator::Join(c) | JoinOperator::Inner(c) => (Some(c), 0),
                JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (Some(c), 0),
                JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (Some(c), 1),
                JoinOperator::FullOuter(c) => (Some(c), 2),
                _ => (None, 0),
            };
            let merged: Vec<String> = match constraint {
                Some(JoinConstraint::Using(names)) => names
                    .iter()
                    .filter_map(|n| n.0.last().and_then(|p| p.as_ident()).map(crate::catalog::ident))
                    .collect(),
                Some(JoinConstraint::Natural) => out
                    .iter()
                    .map(|(n, _)| n.clone())
                    .filter(|n| right.iter().any(|(r, _)| r == n))
                    .collect(),
                _ => Vec::new(),
            };
            let item_of = |list: &[(String, SelectItem)], n: &str| {
                list.iter().find(|(c, _)| c == n).map(|(_, i)| i.clone())
            };
            let expr_of = |i: &SelectItem| match i {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    e.to_string()
                }
                other => other.to_string(),
            };
            let mut next = Vec::new();
            for m in &merged {
                let (Some(l), Some(r)) = (item_of(&out, m), item_of(&right, m)) else {
                    return Err(Error::new(format!(
                        "column \"{m}\" specified in USING clause does not exist"
                    )));
                };
                let item = match side {
                    0 => l,
                    1 => r,
                    _ => select_list(&format!(
                        "COALESCE({}, {}) AS {}",
                        expr_of(&l),
                        expr_of(&r),
                        super::relations::quote(m)
                    ))?
                    .remove(0),
                };
                next.push((m.clone(), item));
            }
            next.extend(out.into_iter().filter(|(c, _)| !merged.contains(c)));
            next.extend(right.into_iter().filter(|(c, _)| !merged.contains(c)));
            out = next;
        }
        Ok(out)
    };
    for item in std::mem::take(&mut s.projection) {
        match &item {
            SelectItem::Wildcard(_) if merged => {
                for t in &s.from {
                    projection.extend(joined(t)?.into_iter().map(|(_, i)| i));
                }
            }
            SelectItem::Wildcard(_) => {
                for (by, settings) in &from {
                    match (by, settings) {
                        (Some(by), true) => projection.extend(columns(by)?),
                        (Some(by), false) if !merged => projection.extend(select_list(&format!(
                            "{}.*",
                            super::relations::quote(by)
                        ))?),
                        _ => {
                            return Err(Error::new(
                                "* is not read beside pg_settings here: a server's own \
                                 setting's values are refused where they are read, so name each \
                                 column",
                            ))
                        }
                    }
                }
            }
            SelectItem::QualifiedWildcard(
                sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(n),
                _,
            ) if crate::catalog::object_name(n) == relation => projection.extend(columns(&relation)?),
            _ => projection.push(item),
        }
    }
    s.projection = projection;
    let mut wrap = Wrap {
        relation: &relation,
        from: Vec::new(),
        inside: 0,
        failed: None,
    };
    for item in &mut s.projection {
        let alone = match item {
            SelectItem::UnnamedExpr(e) => value_column(e, &relation),
            _ => None,
        };
        let _ = item.visit(&mut wrap);
        if let (Some(c), SelectItem::UnnamedExpr(e)) = (alone, &*item) {
            *item = SelectItem::ExprWithAlias {
                expr: e.clone(),
                alias: sqlparser::ast::Ident::new(c),
            };
        }
    }
    let _ = s.selection.visit(&mut wrap);
    let _ = s.having.visit(&mut wrap);
    let _ = s.group_by.visit(&mut wrap);
    let _ = s.distinct.visit(&mut wrap);
    let _ = s.named_window.visit(&mut wrap);
    for t in &mut s.from {
        for j in &mut t.joins {
            let _ = j.join_operator.visit(&mut wrap);
        }
    }
    let _ = q.order_by.visit(&mut wrap);
    match wrap.failed {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// A select list's items, as written.
fn select_list(text: &str) -> Result<Vec<SelectItem>> {
    let parsed = crate::read::image::parse_sql(&format!("SELECT {text}"))?;
    match parsed.as_slice() {
        [sqlparser::ast::Statement::Query(q)] => match q.body.as_ref() {
            SetExpr::Select(s) => Ok(s.projection.clone()),
            _ => Err(Error::new(format!("{text} is no select list"))),
        },
        _ => Err(Error::new(format!("{text} is no select list"))),
    }
}

/// The value column of pg_settings, by the name it goes by, that an expression names: bare or
/// after that name.
fn value_column(e: &Expr, relation: &str) -> Option<String> {
    let c = match e {
        Expr::Identifier(i) => crate::catalog::ident(i),
        Expr::CompoundIdentifier(p) => {
            let names: Vec<String> = p.iter().map(crate::catalog::ident).collect();
            match names.as_slice() {
                [r, c] if r == relation => c.clone(),
                [s, r, c] if s == "pg_catalog" && r == relation && r == "pg_settings" => c.clone(),
                _ => return None,
            }
        }
        _ => return None,
    };
    super::settings::VALUES.contains(&c.as_str()).then_some(c)
}

/// A value column of pg_settings read through the refusal of a server's own setting, by its row's
/// name.
fn value_read(e: &Expr, relation: &str, column: &str) -> Result<Expr> {
    let read = select_list(&format!(
        "\"{}\"({}.name, '{column}', {e})",
        super::settings::VALUE,
        super::relations::quote(relation)
    ))?;
    match read.into_iter().next() {
        Some(SelectItem::UnnamedExpr(x)) => Ok(x),
        _ => Err(Error::new(format!("{e} is not read"))),
    }
}

/// Each expression reading a value column of pg_settings read through `value_read`, and its whole
/// row refused.
struct Wrap<'a> {
    relation: &'a str,
    /// For each subquery entered, whether a FROM of its own stands in it.
    from: Vec<bool>,
    /// How many values read through their refusal the expression is inside.
    inside: usize,
    failed: Option<Error>,
}

impl sqlparser::ast::VisitorMut for Wrap<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        self.from
            .push(!matches!(q.body.as_ref(), SetExpr::Select(s) if s.from.is_empty()));
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
        self.from.pop();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        // A value already read through its refusal is read so once.
        if let Expr::Function(f) = e {
            if function_name(f) == super::settings::VALUE {
                self.inside += 1;
            }
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        if let Expr::Function(f) = e {
            if function_name(f) == super::settings::VALUE {
                self.inside -= 1;
                return ControlFlow::Continue(());
            }
        }
        if self.inside > 0 {
            return ControlFlow::Continue(());
        }
        let bare = !self.from.iter().any(|f| *f);
        let column = |n: &str| super::settings::columns().iter().any(|(c, _)| *c == n);
        match e {
            Expr::Identifier(_) if !bare => {}
            Expr::Identifier(i)
                if crate::catalog::ident(i) == self.relation
                    && !column(&crate::catalog::ident(i)) =>
            {
                self.failed.get_or_insert(Error::new(format!(
                    "a whole row of pg_settings is not read: a server's own setting's values in \
                     it are refused where they are read, so name each column of {}",
                    self.relation
                )));
            }
            _ => {
                if let Some(c) = value_column(e, self.relation) {
                    match value_read(e, self.relation, &c) {
                        Ok(x) => *e = x,
                        Err(err) => {
                            self.failed.get_or_insert(err);
                        }
                    }
                }
            }
        }
        ControlFlow::Continue(())
    }
}

/// Each output column's refusal: a select item reading what only a server fills is NULL. Then,
/// throughout, the privilege tests and the temporary schemas answered, the domains read as their
/// base types, information_schema's functions inlined.
fn rewrite(q: &mut Query, catalog: &crate::catalog::Catalog) -> Result<Vec<Option<String>>> {
    let mut refused: Vec<Option<String>> = Vec::new();
    // A privilege test holds for whoever opens a file, so what it reads decides no column.
    let _ = sqlparser::ast::visit_expressions_mut(q, |e| {
        if let Expr::Function(f) = e {
            if privilege_test(&function_name(f)) {
                *e = Expr::Value(Value::Boolean(true).into());
            }
        }
        ControlFlow::<()>::Continue(())
    });
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
        let through = refused_beneath(catalog, s);
        for (i, item) in s.projection.iter_mut().enumerate() {
            let e = match item {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                _ => continue,
            };
            if let Some(why) = servers_read(e).or_else(|| read_beneath(e, &through)) {
                if refused.len() <= i {
                    refused.resize(i + 1, None);
                }
                refused[i].get_or_insert(why);
                // A column keeps its name once it is NULL, as the view names it. An unnamed
                // function keeps none: PostgreSQL names it by its function, which the view writes
                // with an alias wherever one is read.
                let column = match e {
                    Expr::Identifier(c) => Some(c.clone()),
                    Expr::CompoundIdentifier(p) => p.last().cloned(),
                    _ => None,
                };
                *e = Expr::Value(Value::Null.into());
                if let (SelectItem::UnnamedExpr(_), Some(alias)) = (&*item, column) {
                    *item = SelectItem::ExprWithAlias {
                        expr: Expr::Value(Value::Null.into()),
                        alias,
                    };
                }
            }
        }
    }
    let mut failed: Option<Error> = None;
    let _ = sqlparser::ast::visit_expressions_mut(q, |e| {
        if let Err(err) = answer(e, catalog) {
            failed.get_or_insert(err);
        }
        ControlFlow::<()>::Continue(())
    });
    if let Some(e) = failed {
        return Err(e);
    }
    expand_arrays(q)?;
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

/// `information_schema._pg_expandarray(a) AS x` in a select list, each element of `a` with its
/// place counted from 1, as the function returns them a row each: read as `unnest(a) WITH
/// ORDINALITY`, LATERAL, after the FROM of its query, the element and its place the query's columns
/// `snail_x_x` and `snail_x_n`, the place an integer as the function declares it. Each `(q.x).x`
/// and `(q.x).n` reads them.
fn expand_arrays(q: &mut Query) -> Result<()> {
    struct Expand {
        names: Vec<String>,
        failed: Option<Error>,
    }
    impl sqlparser::ast::VisitorMut for Expand {
        type Break = ();

        fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
            if let Err(e) = expand_in(&mut q.body, &mut self.names) {
                self.failed.get_or_insert(e);
            }
            ControlFlow::Continue(())
        }
    }
    let mut expand = Expand {
        names: Vec::new(),
        failed: None,
    };
    let _ = sqlparser::ast::VisitMut::visit(q, &mut expand);
    if let Some(e) = expand.failed {
        return Err(e);
    }
    let names = expand.names;
    if names.is_empty() {
        return Ok(());
    }
    let _ = sqlparser::ast::visit_expressions_mut(q, |x| {
        let Expr::CompoundFieldAccess { root, access_chain } = x else {
            return ControlFlow::<()>::Continue(());
        };
        let mut r = root.as_ref();
        while let Expr::Nested(inner) = r {
            r = inner;
        }
        let (qualifier, column) = match r {
            Expr::CompoundIdentifier(p) if p.len() == 2 => (Some(p[0].clone()), &p[1]),
            Expr::Identifier(i) => (None, i),
            _ => return ControlFlow::Continue(()),
        };
        let [sqlparser::ast::AccessExpr::Dot(Expr::Identifier(field))] = access_chain.as_slice()
        else {
            return ControlFlow::Continue(());
        };
        if !names.contains(&column.value) || !matches!(field.value.as_str(), "x" | "n") {
            return ControlFlow::Continue(());
        }
        let read = sqlparser::ast::Ident::new(format!("snail_{}_{}", column.value, field.value));
        *x = match qualifier {
            Some(q) => Expr::CompoundIdentifier(vec![q, read]),
            None => Expr::Identifier(read),
        };
        ControlFlow::Continue(())
    });
    Ok(())
}

fn expand_in(body: &mut SetExpr, names: &mut Vec<String>) -> Result<()> {
    match body {
        SetExpr::Select(s) => expand_select(s, names),
        SetExpr::SetOperation { left, right, .. } => {
            expand_in(left, names)?;
            expand_in(right, names)
        }
        _ => Ok(()),
    }
}

fn expand_select(s: &mut sqlparser::ast::Select, names: &mut Vec<String>) -> Result<()> {
    let mut items = Vec::with_capacity(s.projection.len());
    for item in std::mem::take(&mut s.projection) {
        let expanded = match &item {
            SelectItem::ExprWithAlias {
                expr: Expr::Function(f),
                alias,
            } if function_name(f) == "information_schema._pg_expandarray" => {
                match arguments(f).as_slice() {
                    [a] => Some((a.clone(), alias.value.clone())),
                    _ => None,
                }
            }
            _ => None,
        };
        let Some((array, name)) = expanded else {
            items.push(item);
            continue;
        };
        let lateral = format!("snail_{name}");
        let parsed = crate::read::image::parse_sql(&format!(
            "SELECT {lateral}.x AS snail_{name}_x, CAST({lateral}.n AS integer) AS snail_{name}_n \
             FROM snail_t CROSS JOIN LATERAL unnest({array}) WITH ORDINALITY AS {lateral}(x, n)"
        ))?;
        let [sqlparser::ast::Statement::Query(made)] = parsed.as_slice() else {
            return Err(Error::new("_pg_expandarray's elements are no question"));
        };
        let SetExpr::Select(made) = made.body.as_ref() else {
            return Err(Error::new("_pg_expandarray's elements are no question"));
        };
        items.extend(made.projection.iter().cloned());
        s.from.push(sqlparser::ast::TableWithJoins {
            relation: made.from[0].joins[0].relation.clone(),
            joins: Vec::new(),
        });
        names.push(name);
    }
    s.projection = items;
    Ok(())
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
fn answer(e: &mut Expr, catalog: &crate::catalog::Catalog) -> Result<()> {
    let release = catalog.release;
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
        // What a write takes: a table's rows, and a view's where it is written through, each
        // view's as `updatable::writable` finds it.
        "pg_relation_is_updatable" | "pg_column_is_updatable" => {
            let args = arguments(f);
            let Some(rel) = args.first() else {
                return Ok(());
            };
            let views = super::updatable::writable_views(catalog)?;
            let what = match (name.as_str(), args.get(1)) {
                ("pg_relation_is_updatable", _) => {
                    let events: String = views
                        .iter()
                        .filter(|(_, can, _)| *can > 0)
                        .map(|(oid, can, _)| format!(" WHEN {oid} THEN {can}"))
                        .collect();
                    // A CASE takes at least one WHEN: where no view is written through, none.
                    let viewed = match events.is_empty() {
                        true => "0".to_string(),
                        false => format!("CASE CAST(snail_rel.oid AS bigint){events} ELSE 0 END"),
                    };
                    format!("CASE WHEN snail_rel.relkind IN ('r', 'p') THEN 28 ELSE {viewed} END")
                }
                (_, Some(attnum)) => {
                    let set: String = views
                        .iter()
                        .filter(|(_, _, set)| !set.is_empty())
                        .map(|(oid, _, set)| {
                            let numbers: Vec<String> = set.iter().map(|n| n.to_string()).collect();
                            format!(
                                " OR (snail_rel.oid = {oid} AND ({attnum}) IN ({}))",
                                numbers.join(", ")
                            )
                        })
                        .collect();
                    format!("(snail_rel.relkind IN ('r', 'p') AND ({attnum}) > 0){set}")
                }
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
            if let Err(err) = answer(x, catalog) {
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

/// Whether a word names one of information_schema's functions, whose bodies read the catalog.
pub(crate) fn names_schema_function(word: &str) -> bool {
    word.strip_prefix("information_schema.")
        .is_some_and(|f| f.starts_with("_pg_"))
}

/// A question's own calls of information_schema's functions, read as its views read them: each
/// inlined as its body, `_pg_expandarray` in FROM as `unnest … WITH ORDINALITY` with its place an
/// integer, and in a select list as a view writes it. None where it calls none.
pub(crate) fn inlined_calls(catalog: &crate::catalog::Catalog, q: &Query) -> Result<Option<Query>> {
    let lower = q.to_string().to_lowercase();
    if !lower.contains("information_schema._pg_") {
        return Ok(None);
    }
    let mut q = Box::new(q.clone());
    struct Froms {
        called: bool,
        failed: Option<Error>,
    }
    impl sqlparser::ast::VisitorMut for Froms {
        type Break = ();
        fn pre_visit_table_factor(
            &mut self,
            t: &mut sqlparser::ast::TableFactor,
        ) -> ControlFlow<()> {
            use sqlparser::ast::TableFactor;
            let TableFactor::Table {
                name,
                args: Some(args),
                alias,
                ..
            } = t
            else {
                return ControlFlow::Continue(());
            };
            let n = crate::catalog::object_name(name).to_lowercase();
            if n != "information_schema._pg_expandarray" {
                return ControlFlow::Continue(());
            }
            let [FunctionArg::Unnamed(FunctionArgExpr::Expr(array))] = args.args.as_slice() else {
                return ControlFlow::Continue(());
            };
            let made = crate::read::image::parse_sql(&format!(
                "SELECT snail_u.x, CAST(snail_u.n AS integer) AS n \
                 FROM unnest({array}) WITH ORDINALITY AS snail_u(x, n)"
            ));
            match made.as_deref() {
                Ok([sqlparser::ast::Statement::Query(sub)]) => {
                    let alias = alias.take().unwrap_or_else(|| sqlparser::ast::TableAlias {
                        explicit: true,
                        name: sqlparser::ast::Ident::new("_pg_expandarray"),
                        columns: Vec::new(),
                        at: None,
                    });
                    // A function in FROM reads the relations before it, as LATERAL does.
                    *t = TableFactor::Derived {
                        lateral: true,
                        subquery: sub.clone(),
                        alias: Some(alias),
                        sample: None,
                    };
                    self.called = true;
                }
                Ok(_) => {}
                Err(e) => {
                    self.failed.get_or_insert(Error::new(e.to_string()));
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut froms = Froms {
        called: false,
        failed: None,
    };
    let _ = sqlparser::ast::VisitMut::visit(q.as_mut(), &mut froms);
    if let Some(e) = froms.failed {
        return Err(e);
    }
    let mut called = froms.called;
    let mut failed: Option<Error> = None;
    let _ = sqlparser::ast::visit_expressions_mut(q.as_mut(), |e| {
        if let Expr::Function(f) = e {
            let name = function_name(f);
            let own = name.strip_prefix("information_schema.");
            if own.is_some_and(|o| o != "_pg_expandarray") {
                called = true;
                if let Err(err) = answer(e, catalog) {
                    failed.get_or_insert(err);
                }
            }
        }
        ControlFlow::<()>::Continue(())
    });
    if let Some(e) = failed {
        return Err(e);
    }
    // The bodies' own `_pg_expandarray`, and the question's in a select list.
    expand_arrays(q.as_mut())?;
    called |= lower.contains("_pg_expandarray");
    Ok(called.then_some(*q))
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

/// Each relation a function's body names under an alias, named afresh, `snail_body_<alias>`, with
/// every column the body qualifies by it.
fn own_names(q: &mut Query) {
    struct Aliases(Vec<String>);
    impl sqlparser::ast::VisitorMut for Aliases {
        type Break = ();

        fn pre_visit_table_factor(
            &mut self,
            t: &mut sqlparser::ast::TableFactor,
        ) -> ControlFlow<()> {
            use sqlparser::ast::TableFactor as T;
            if let T::Table { alias: Some(a), .. } | T::Derived { alias: Some(a), .. } = t {
                let own = ident_folded(&a.name);
                a.name = sqlparser::ast::Ident::new(format!("snail_body_{own}"));
                self.0.push(own);
            }
            ControlFlow::Continue(())
        }
    }
    let mut aliases = Aliases(Vec::new());
    let _ = sqlparser::ast::VisitMut::visit(q, &mut aliases);
    let _ = sqlparser::ast::visit_expressions_mut(q, |x| {
        if let Expr::CompoundIdentifier(p) = x {
            if p.len() >= 2 && aliases.0.contains(&ident_folded(&p[0])) {
                p[0] = sqlparser::ast::Ident::new(format!("snail_body_{}", ident_folded(&p[0])));
            }
        }
        ControlFlow::<()>::Continue(())
    });
}

/// One of information_schema's functions as the expression it returns, its arguments in place of
/// its parameters: `($2).typtype` the second argument's relation's column. A body of BEGIN ATOMIC
/// holding one SELECT is that question, a scalar subquery, the relations it names under names of
/// their own so that none hides one an argument names.
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
    let params: Vec<String> = key
        .split_once('(')
        .map(|(_, p)| p.trim_end_matches(')'))
        .unwrap_or("")
        .split(", ")
        .map(|p| p.split(' ').next().unwrap_or("").to_string())
        .collect();
    let no_body = || Error::new(format!("information_schema.{name}'s body is no expression"));
    let mut e = match text.split_once("\nRETURN ") {
        Some((_, body)) => expression(&super::spelled::spelled(body.trim())?)?,
        None => {
            let body = text
                .split_once("\nBEGIN ATOMIC\n")
                .and_then(|(_, b)| b.trim_end().strip_suffix("END"))
                .and_then(|b| b.trim_end().strip_suffix(';'))
                .ok_or_else(no_body)?;
            let parsed = crate::read::image::parse_sql(&super::spelled::spelled(body)?)?;
            let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
                return Err(no_body());
            };
            let mut q = (**q).clone();
            own_names(&mut q);
            Expr::Subquery(Box::new(q))
        }
    };
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
