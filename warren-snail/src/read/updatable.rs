//! A write through a view, as PostgreSQL's rewriter writes through an automatically updatable view:
//! the view reads one relation, a table or such a view, with no DISTINCT, grouping, set operation,
//! WITH, LIMIT, aggregate, window or set-returning function; the write goes to that relation, each
//! of the view's columns the relation's column or expression it is, the view's WHERE joined to the
//! write's. A view's WITH CHECK OPTION is a condition each new row must meet: CASCADED its own and
//! every view's under it, LOCAL its own.

use super::pipeline::Value;
use crate::catalog::{ident, object_name, Catalog, View};
use crate::oids::{full, Object};
use crate::{Error, Result};
use std::ops::ControlFlow;
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, Statement as Sql,
    TableFactor, VisitMut, VisitorMut,
};

/// What a view reads, where a write can go through it.
pub(crate) struct Through {
    /// The relation it reads, by the name the catalog holds it by.
    pub base: String,
    /// Each of its columns by name, and the relation's column or expression it is, every qualifier
    /// naming the relation taken off.
    pub columns: Vec<(String, Expr)>,
    /// Its WHERE over the relation's columns.
    pub condition: Option<Expr>,
}

/// What a view reads where a write can go through it, or why it cannot, in the words of
/// PostgreSQL's hint.
pub(crate) fn through(
    catalog: &Catalog,
    view: &View,
) -> Result<std::result::Result<Through, &'static str>> {
    let q = match super::image::parse_sql(&view.query)?.pop() {
        Some(Sql::Query(q)) => q,
        _ => return Err(Error::new(format!("{}: its question reads as no question", view.name))),
    };
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Ok(Err(
            "Views containing UNION, INTERSECT, or EXCEPT are not automatically updatable.",
        ));
    };
    if s.distinct.is_some() {
        return Ok(Err("Views containing DISTINCT are not automatically updatable."));
    }
    if !matches!(&s.group_by, GroupByExpr::Expressions(g, _) if g.is_empty()) {
        return Ok(Err("Views containing GROUP BY are not automatically updatable."));
    }
    if s.having.is_some() {
        return Ok(Err("Views containing HAVING are not automatically updatable."));
    }
    if q.with.is_some() {
        return Ok(Err("Views containing WITH are not automatically updatable."));
    }
    if q.limit_clause.is_some() || q.fetch.is_some() {
        return Ok(Err(
            "Views containing LIMIT or OFFSET are not automatically updatable.",
        ));
    }
    let items: Vec<&Expr> = s
        .projection
        .iter()
        .filter_map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
            _ => None,
        })
        .collect();
    let calls: Vec<(bool, bool, bool)> = items.iter().map(|e| super::pipeline::calls_in(e)).collect();
    if calls.iter().any(|c| c.0) {
        return Ok(Err(
            "Views that return aggregate functions are not automatically updatable.",
        ));
    }
    if calls.iter().any(|c| c.1) {
        return Ok(Err(
            "Views that return window functions are not automatically updatable.",
        ));
    }
    if calls.iter().any(|c| c.2) {
        return Ok(Err(
            "Views that return set-returning functions are not automatically updatable.",
        ));
    }
    let single = match s.from.as_slice() {
        [f] if f.joins.is_empty() => match &f.relation {
            TableFactor::Table {
                name, alias, args: None, ..
            } => Some((object_name(name), alias.as_ref().map(|a| ident(&a.name)))),
            _ => None,
        },
        _ => None,
    };
    let Some((base, alias)) = single.filter(|(b, _)| {
        catalog.tables.iter().any(|t| t.name == *b) || catalog.views.iter().any(|v| v.name == *b)
    }) else {
        return Ok(Err(
            "Views that do not select from a single table or view are not automatically updatable.",
        ));
    };
    // Its relation named as the view's question names it: by its alias, its name, or its schema
    // and name.
    let bare = base.rsplit('.').next().unwrap_or(&base).to_string();
    let names: Vec<String> = match &alias {
        Some(a) => vec![a.clone()],
        None => vec![bare.clone(), base.clone()],
    };
    let unqualified = |e: &Expr| -> Expr {
        let mut e = e.clone();
        let _ = sqlparser::ast::visit_expressions_mut(&mut e, |x| {
            if let Expr::CompoundIdentifier(parts) = x {
                if let Some((c, q)) = parts.split_last() {
                    let q = q.iter().map(ident).collect::<Vec<_>>().join(".");
                    if names.contains(&q) {
                        *x = Expr::Identifier(c.clone());
                    }
                }
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        e
    };
    let named = super::view_columns(catalog, &q)?;
    let mut exprs = Vec::new();
    for i in &s.projection {
        match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                exprs.push(unqualified(e))
            }
            // A star, the relation's columns, each by its name.
            _ => {
                let of = relation_columns(catalog, &base);
                exprs.extend(of.into_iter().map(|c| Expr::Identifier(Ident::with_quote('"', c))));
            }
        }
    }
    let columns = view
        .columns
        .iter()
        .cloned()
        .chain(named.into_iter().skip(view.columns.len()))
        .zip(exprs)
        .collect();
    Ok(Ok(Through {
        base,
        columns,
        condition: s.selection.as_ref().map(unqualified),
    }))
}

/// The columns of a table or a view, by name, in order.
pub(crate) fn relation_columns(catalog: &Catalog, name: &str) -> Vec<String> {
    if let Some(t) = catalog.tables.iter().find(|t| t.name == name) {
        return t.columns.iter().map(|c| c.name.clone()).collect();
    }
    catalog
        .views
        .iter()
        .find(|v| v.name == name)
        .and_then(|v| {
            let q = match super::image::parse_sql(&v.query).ok()?.pop()? {
                Sql::Query(q) => q,
                _ => return None,
            };
            let made = super::view_columns(catalog, &q).ok()?;
            Some(v.columns.iter().cloned().chain(made.into_iter().skip(v.columns.len())).collect())
        })
        .unwrap_or_default()
}

/// What a write to a relation can do, as `pg_relation_is_updatable` counts it (INSERT 8, UPDATE 4,
/// DELETE 16), and the columns an INSERT or UPDATE can set, by name. A table takes every write
/// and every column; a view none where it cannot be written through or what it reads cannot, its
/// columns those that are a settable column of its relation, DELETE alone where none is.
pub(crate) fn writable(catalog: &Catalog, name: &str) -> Result<(u32, Vec<String>)> {
    if catalog.tables.iter().any(|t| t.name == name) {
        return Ok((28, relation_columns(catalog, name)));
    }
    let Some(view) = catalog.views.iter().find(|v| v.name == name) else {
        return Ok((0, Vec::new()));
    };
    let Ok(t) = through(catalog, view)? else {
        return Ok((0, Vec::new()));
    };
    let (under, settable) = writable(catalog, &t.base)?;
    if under == 0 {
        return Ok((0, Vec::new()));
    }
    let set: Vec<String> = t
        .columns
        .iter()
        .filter(|(_, e)| matches!(e, Expr::Identifier(i) if settable.contains(&ident(i))))
        .map(|(n, _)| n.clone())
        .collect();
    Ok((if set.is_empty() { 16 } else { 28 }, set))
}

/// Each view of the file by its OID, with what `writable` finds of it: what a write to it can do,
/// and the columns it can set, numbered from 1.
pub(crate) fn writable_views(catalog: &Catalog) -> Result<Vec<(u32, u32, Vec<usize>)>> {
    let mut out = Vec::new();
    for v in &catalog.views {
        let Some(oid) = catalog.oids.of(&Object::Class(full(&v.name))) else {
            continue;
        };
        let (can, set) = writable(catalog, &v.name)?;
        let numbers = relation_columns(catalog, &v.name)
            .iter()
            .enumerate()
            .filter(|(_, c)| set.contains(c))
            .map(|(i, _)| i + 1)
            .collect();
        out.push((oid, can, numbers));
    }
    Ok(out)
}

/// A view's name, as a write names it.
fn view_named<'c>(catalog: &'c Catalog, name: &ObjectName) -> Option<&'c View> {
    let n = object_name(name);
    catalog.views.iter().find(|v| v.name == n)
}

/// Whether a write names a view as the relation it writes.
pub(crate) fn writes_a_view(catalog: &Catalog, st: &Sql) -> bool {
    target(st).is_some_and(|n| view_named(catalog, n).is_some())
}

/// The relation a write names.
fn target(st: &Sql) -> Option<&ObjectName> {
    match st {
        Sql::Insert(i) => match &i.table {
            sqlparser::ast::TableObject::TableName(n) => Some(n),
            _ => None,
        },
        Sql::Update(u) => match &u.table.relation {
            TableFactor::Table { name, .. } => Some(name),
            _ => None,
        },
        Sql::Delete(d) => match &d.from {
            sqlparser::ast::FromTable::WithFromKeyword(f)
            | sqlparser::ast::FromTable::WithoutKeyword(f) => match f.as_slice() {
                [t] => match &t.relation {
                    TableFactor::Table { name, .. } => Some(name),
                    _ => None,
                },
                _ => None,
            },
        },
        _ => None,
    }
}

/// The refusal of a write to a view's column that is no column of its relation.
fn not_a_column(what: &str, column: &str, view: &str) -> Error {
    Error::new(format!(
        "cannot {what} column \"{column}\" of view \"{view}\": View columns that are not columns of \
         their base relation are not updatable."
    ))
}

/// A write to a view written as the write to the table under it, through every view between, and
/// the conditions its new rows must meet, each with the view it is the CHECK OPTION of, innermost
/// first. The relation is given the name the write gives the view, so a FROM or USING beside it
/// meets no second relation of that name; an ON CONFLICT's update meets a row the view does not
/// show, the view's WHERE narrowing only what UPDATE and DELETE find.
pub(crate) fn rewritten(catalog: &Catalog, st: &Sql) -> Result<(Sql, Vec<(String, Expr)>)> {
    let mut st = st.clone();
    let mut checks: Vec<(String, Expr)> = Vec::new();
    let mut cascading = false;
    while let Some(view) = target(&st).and_then(|n| view_named(catalog, n)).cloned() {
        let bare = view.name.rsplit('.').next().unwrap_or(&view.name).to_string();
        let t = through(catalog, &view)?.map_err(|hint| {
            let what = match &st {
                Sql::Insert(_) => "insert into",
                Sql::Update(_) => "update",
                _ => "delete from",
            };
            Error::new(format!("cannot {what} view \"{bare}\": {hint}"))
        })?;
        // The name the write gives the view: its alias, or its name.
        let alias: Option<Ident> = match &st {
            Sql::Insert(i) => i.table_alias.as_ref().map(|a| a.alias.clone()),
            Sql::Update(u) => match &u.table.relation {
                TableFactor::Table { alias: Some(a), .. } => Some(a.name.clone()),
                _ => None,
            },
            Sql::Delete(d) => match &d.from {
                sqlparser::ast::FromTable::WithFromKeyword(f)
                | sqlparser::ast::FromTable::WithoutKeyword(f) => match f.first().map(|f| &f.relation) {
                    Some(TableFactor::Table { alias: Some(a), .. }) => Some(a.name.clone()),
                    _ => None,
                },
            },
            _ => None,
        };
        let by: Vec<String> = match &alias {
            Some(a) => vec![ident(a)],
            None => vec![bare.clone(), view.name.clone()],
        };
        let to = alias.clone().unwrap_or_else(|| Ident::with_quote('"', bare.clone()));
        let named_as = sqlparser::ast::TableAlias {
            explicit: true,
            name: to.clone(),
            columns: vec![],
            at: None,
        };
        let column = |name: &str| t.columns.iter().find(|(n, _)| n == name).map(|(_, e)| e);
        // The column of the relation a view's column is, where it is one.
        let simple = |name: &str| -> Option<Ident> {
            match column(name) {
                Some(Expr::Identifier(i)) => Some(i.clone()),
                _ => None,
            }
        };
        // A view none of whose columns is its relation's takes a DELETE alone.
        if !t.columns.iter().any(|(_, e)| matches!(e, Expr::Identifier(_))) {
            let what = match &st {
                Sql::Insert(_) => Some("insert into"),
                Sql::Update(_) => Some("update"),
                _ => None,
            };
            if let Some(what) = what {
                return Err(Error::new(format!(
                    "cannot {what} view \"{bare}\": Views that have no updatable columns are not \
                     automatically updatable."
                )));
            }
        }
        let relation = ObjectName(
            t.base
                .split('.')
                .map(|p| ObjectNamePart::Identifier(Ident::with_quote('"', p)))
                .collect(),
        );
        // Each condition the view's CHECK OPTION or one above it asks, read over the relation.
        for (_, c) in checks.iter_mut() {
            *c = mapped(c, &t.columns, &[], None, &[]);
        }
        cascading |= view.check_option == Some("cascaded");
        if let (Some(c), true) = (&t.condition, view.check_option.is_some() || cascading) {
            checks.push((bare.clone(), c.clone()));
        }
        let joined = |w: Option<&Expr>, kept: &[&str]| -> Option<Expr> {
            let c = t.condition.as_ref().map(|c| under(c, Some(&to)));
            match (c, w.map(|w| mapped(w, &t.columns, &by, Some(&to), kept))) {
                (Some(c), Some(w)) => Some(Expr::BinaryOp {
                    left: Box::new(Expr::Nested(Box::new(c))),
                    op: sqlparser::ast::BinaryOperator::And,
                    right: Box::new(Expr::Nested(Box::new(w))),
                }),
                (c, w) => c.or(w),
            }
        };
        let returning = |items: &Option<Vec<SelectItem>>,
                         beside: &[sqlparser::ast::TableWithJoins],
                         to: Option<&Ident>|
         -> Option<Vec<SelectItem>> {
            let spread = |q: Option<&Ident>| -> Vec<SelectItem> {
                t.columns
                    .iter()
                    .map(|(n, e)| SelectItem::ExprWithAlias {
                        expr: Expr::Nested(Box::new(under(e, q))),
                        alias: Ident::with_quote('"', n.clone()),
                    })
                    .collect()
            };
            items.as_ref().map(|items| {
                items
                    .iter()
                    .flat_map(|i| match i {
                        // The view's columns, then each relation beside it.
                        SelectItem::Wildcard(_) => spread(to)
                            .into_iter()
                            .chain(relations_beside(beside).into_iter().map(|n| {
                                SelectItem::QualifiedWildcard(
                                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(n),
                                    Default::default(),
                                )
                            }))
                            .collect(),
                        SelectItem::QualifiedWildcard(
                            sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(n),
                            _,
                        ) => {
                            let q = object_name(n);
                            match n.0.as_slice() {
                                [ObjectNamePart::Identifier(p)] if KEPT.contains(&ident(p).as_str()) => {
                                    spread(Some(p))
                                }
                                _ if by.contains(&q) => spread(to),
                                _ => vec![i.clone()],
                            }
                        }
                        SelectItem::UnnamedExpr(e) => vec![SelectItem::ExprWithAlias {
                            expr: mapped(e, &t.columns, &by, to, &KEPT),
                            alias: Ident::with_quote('"', super::pipeline::default_name(e)),
                        }],
                        SelectItem::ExprWithAlias { expr, alias } => vec![SelectItem::ExprWithAlias {
                            expr: mapped(expr, &t.columns, &by, to, &KEPT),
                            alias: alias.clone(),
                        }],
                        other => vec![other.clone()],
                    })
                    .collect()
            })
        };
        match &mut st {
            Sql::Insert(i) => {
                let given: Vec<String> = match i.columns.is_empty() {
                    true => t.columns.iter().map(|(n, _)| n.clone()).collect(),
                    false => i.columns.iter().map(object_name).collect(),
                };
                let mut columns = Vec::new();
                for c in &given {
                    if column(c).is_none() {
                        return Err(Error::new(format!(
                            "column \"{c}\" of relation \"{bare}\" does not exist"
                        )));
                    }
                    let to_column = simple(c).ok_or_else(|| not_a_column("insert into", c, &bare))?;
                    columns.push(ObjectName(vec![ObjectNamePart::Identifier(to_column)]));
                }
                if let Some(sqlparser::ast::OnInsert::OnConflict(on)) = &mut i.on {
                    match &mut on.conflict_target {
                        Some(sqlparser::ast::ConflictTarget::Columns(cs)) => {
                            for c in cs.iter_mut() {
                                if column(&ident(c)).is_none() {
                                    return Err(Error::new(format!(
                                        "column \"{}\" does not exist",
                                        ident(c)
                                    )));
                                }
                                *c = simple(&ident(c)).ok_or_else(|| {
                                    Error::new(
                                        "there is no unique or exclusion constraint matching the \
                                         ON CONFLICT specification",
                                    )
                                })?;
                            }
                        }
                        // A view holds no constraint.
                        Some(sqlparser::ast::ConflictTarget::OnConstraint(n)) => {
                            return Err(Error::new(format!(
                                "constraint \"{}\" for table \"{bare}\" does not exist",
                                object_name(n)
                            )))
                        }
                        None => {}
                    }
                    if let sqlparser::ast::OnConflictAction::DoUpdate(u) = &mut on.action {
                        let kept = [&["excluded"][..], &KEPT[..]].concat();
                        for a in u.assignments.iter_mut() {
                            let c = assigned(a)?;
                            if column(&c).is_none() {
                                return Err(Error::new(format!(
                                    "column \"{c}\" of relation \"{bare}\" does not exist"
                                )));
                            }
                            let to_column =
                                simple(&c).ok_or_else(|| not_a_column("insert into", &c, &bare))?;
                            a.target = sqlparser::ast::AssignmentTarget::ColumnName(ObjectName(vec![
                                ObjectNamePart::Identifier(to_column),
                            ]));
                            a.value = mapped(&a.value, &t.columns, &by, Some(&to), &kept);
                        }
                        u.selection =
                            u.selection.as_ref().map(|w| mapped(w, &t.columns, &by, Some(&to), &kept));
                    }
                }
                i.returning = returning(&i.returning, &[], None);
                i.columns = columns;
                i.table = sqlparser::ast::TableObject::TableName(relation);
                i.table_alias = Some(sqlparser::ast::TableAliasWithoutColumns {
                    explicit: true,
                    alias: to.clone(),
                });
            }
            Sql::Update(u) => {
                for a in u.assignments.iter_mut() {
                    let c = assigned(a)?;
                    if column(&c).is_none() {
                        return Err(Error::new(format!(
                            "column \"{c}\" of relation \"{bare}\" does not exist"
                        )));
                    }
                    let to_column = simple(&c).ok_or_else(|| not_a_column("update", &c, &bare))?;
                    a.target = sqlparser::ast::AssignmentTarget::ColumnName(ObjectName(vec![
                        ObjectNamePart::Identifier(to_column),
                    ]));
                    a.value = mapped(&a.value, &t.columns, &by, Some(&to), &[]);
                }
                let beside: Vec<sqlparser::ast::TableWithJoins> = match &u.from {
                    Some(sqlparser::ast::UpdateTableFromKind::BeforeSet(f))
                    | Some(sqlparser::ast::UpdateTableFromKind::AfterSet(f)) => f.clone(),
                    None => vec![],
                };
                if let TableFactor::Table { name, alias, .. } = &mut u.table.relation {
                    *name = relation;
                    *alias = Some(named_as.clone());
                }
                u.selection = joined(u.selection.as_ref(), &[]);
                u.returning = returning(&u.returning, &beside, Some(&to));
            }
            Sql::Delete(d) => {
                let beside = d.using.clone().unwrap_or_default();
                let (sqlparser::ast::FromTable::WithFromKeyword(f)
                | sqlparser::ast::FromTable::WithoutKeyword(f)) = &mut d.from;
                if let Some(TableFactor::Table { name, alias, .. }) =
                    f.first_mut().map(|first| &mut first.relation)
                {
                    *name = relation;
                    *alias = Some(named_as.clone());
                }
                d.selection = joined(d.selection.as_ref(), &[]);
                d.returning = returning(&d.returning, &beside, Some(&to));
            }
            _ => unreachable!("a write"),
        }
    }
    checks.reverse();
    Ok((st, checks))
}

/// The qualifiers a RETURNING reads the row before and after by.
const KEPT: [&str; 2] = ["old", "new"];

/// The column an assignment sets, by name.
fn assigned(a: &sqlparser::ast::Assignment) -> Result<String> {
    let sqlparser::ast::AssignmentTarget::ColumnName(n) = &a.target else {
        return Err(Error::new("an UPDATE of a tuple is not written"));
    };
    match n.0.last() {
        Some(ObjectNamePart::Identifier(i)) => Ok(ident(i)),
        _ => Err(Error::new(format!("{n} is not a column"))),
    }
}

/// The name each relation beside a write is read by, its alias or its name.
fn relations_beside(from: &[sqlparser::ast::TableWithJoins]) -> Vec<ObjectName> {
    let named = |f: &TableFactor| -> Option<ObjectName> {
        match f {
            TableFactor::Table { alias: Some(a), .. } | TableFactor::Derived { alias: Some(a), .. } => {
                Some(ObjectName(vec![ObjectNamePart::Identifier(a.name.clone())]))
            }
            TableFactor::Table { name, alias: None, .. } => name
                .0
                .last()
                .map(|p| ObjectName(vec![p.clone()])),
            _ => None,
        }
    };
    from.iter()
        .flat_map(|t| std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)))
        .filter_map(named)
        .collect()
}

/// An expression over a view's columns read over its relation's: a column named alone, or by a
/// name the write gives the view, as the relation's expression under `to`; one named by a
/// qualifier that keeps itself (`excluded`, `old`, `new`), as the relation's expression under
/// that qualifier. Inside a subquery only qualified names are read, its own relations answering
/// the rest.
fn mapped(
    e: &Expr,
    columns: &[(String, Expr)],
    by: &[String],
    to: Option<&Ident>,
    kept: &[&str],
) -> Expr {
    struct Mapping<'a> {
        columns: &'a [(String, Expr)],
        by: &'a [String],
        to: Option<&'a Ident>,
        kept: &'a [&'a str],
        depth: usize,
    }
    impl VisitorMut for Mapping<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, x: &mut Expr) -> ControlFlow<()> {
            let found: Option<(String, Option<Ident>)> = match x {
                Expr::Identifier(i) if self.depth == 0 => Some((ident(i), self.to.cloned())),
                Expr::CompoundIdentifier(parts) => match parts.split_last() {
                    Some((c, q)) => {
                        let named = q.iter().map(ident).collect::<Vec<_>>().join(".");
                        if self.by.contains(&named) {
                            Some((ident(c), self.to.cloned()))
                        } else if q.len() == 1 && self.kept.contains(&named.as_str()) {
                            Some((ident(c), Some(q[0].clone())))
                        } else {
                            None
                        }
                    }
                    None => None,
                },
                _ => None,
            };
            if let Some((name, q)) = found {
                if let Some((_, over)) = self.columns.iter().find(|(n, _)| *n == name) {
                    *x = Expr::Nested(Box::new(under(over, q.as_ref())));
                }
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    let _ = e.visit(&mut Mapping { columns, by, to, kept, depth: 0 });
    e
}

/// An expression over a relation's columns with each column it names alone named under `q`,
/// a subquery's own names left as they are.
fn under(e: &Expr, q: Option<&Ident>) -> Expr {
    struct Under<'a> {
        q: &'a Ident,
        depth: usize,
    }
    impl VisitorMut for Under<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &mut Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn post_visit_expr(&mut self, x: &mut Expr) -> ControlFlow<()> {
            if let (Expr::Identifier(i), 0) = (&*x, self.depth) {
                *x = Expr::CompoundIdentifier(vec![self.q.clone(), i.clone()]);
            }
            ControlFlow::Continue(())
        }
    }
    let mut e = e.clone();
    if let Some(q) = q {
        let _ = e.visit(&mut Under { q, depth: 0 });
    }
    e
}

/// `pg_relation_is_updatable` and `pg_column_is_updatable` called, each as PostgreSQL answers it:
/// over a table, every write and every column numbered past 0; over a view, what `writable` finds;
/// over anything else, or an OID naming nothing, none. NULL for a NULL argument.
pub(crate) fn call(name: &str, args: &[Value]) -> Result<Option<Value>> {
    let column = match name {
        "pg_relation_is_updatable" => false,
        "pg_column_is_updatable" => true,
        _ => return Ok(None),
    };
    if args.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(Some(Value::Null));
    }
    let catalog = crate::deparse::held()?;
    let oid = args.first().and_then(crate::deparse::oid_of).unwrap_or(0);
    let named = match catalog.oids.object(oid) {
        Some(Object::Class(n)) => Some(n.clone()),
        _ => None,
    };
    let table = named
        .as_ref()
        .is_some_and(|n| catalog.tables.iter().any(|t| t.declared() && full(&t.name) == *n));
    let view = named
        .as_ref()
        .and_then(|n| catalog.views.iter().find(|v| full(&v.name) == *n));
    if !column {
        let can = match (table, view) {
            (true, _) => 28,
            (false, Some(v)) => writable(&catalog, &v.name)?.0,
            _ => 0,
        };
        return Ok(Some(Value::Int(i64::from(can))));
    }
    let attnum = match args.get(1) {
        Some(Value::Int(n)) => *n,
        Some(v) => v.text().and_then(|s| s.parse::<i64>().ok()).unwrap_or(0),
        None => 0,
    };
    let can = match (table, view) {
        (true, _) => attnum > 0,
        (false, Some(v)) if attnum > 0 => {
            let (_, set) = writable(&catalog, &v.name)?;
            relation_columns(&catalog, &v.name)
                .get(attnum as usize - 1)
                .is_some_and(|c| set.contains(c))
        }
        _ => false,
    };
    Ok(Some(Value::Bool(can)))
}

/// What `call` answers in: an integer for a relation, a boolean for a column.
pub(crate) fn typed(name: &str) -> Option<crate::catalog::ColumnType> {
    match name {
        "pg_relation_is_updatable" => Some(crate::catalog::ColumnType::Int4),
        "pg_column_is_updatable" => Some(crate::catalog::ColumnType::Boolean),
        _ => None,
    }
}
