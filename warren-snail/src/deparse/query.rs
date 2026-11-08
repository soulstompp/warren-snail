//! A query read as PostgreSQL's parser reads it (analyze.c, parse_clause.c, parse_target.c,
//! parse_relation.c, parse_agg.c, parse_cte.c): its range table, join tree, target list and
//! clauses, each name resolved where the parser resolves it, so that the query printed is the one
//! PostgreSQL prints from the tree it stored.

use super::analyze::{Analyzer, BoolOp, Context, Form, Node};
use super::cat::{self, Cat};
use crate::oids::{full, Object};
use crate::{Error, Result};
use sqlparser::ast as A;
use std::cell::RefCell;

fn not_read(what: &impl std::fmt::Display) -> Error {
    Error::new(format!("{what} is not printed as PostgreSQL prints it"))
}

/// A name the user wrote for a relation, and for its columns.
#[derive(Clone, Debug)]
pub(crate) struct Alias {
    pub name: String,
    pub cols: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JoinKind {
    Inner,
    Left,
    Right,
    Full,
}

/// What a range table entry reads.
#[derive(Clone, Debug)]
pub(crate) enum RteKind {
    /// A table or view: its own name, its schema's OID and its columns' names.
    Relation {
        name: String,
        namespace: u32,
        columns: Vec<String>,
    },
    Subquery(Box<Query>),
    /// A join: each output column's expression, the left and right inputs' columns each output
    /// column comes from (from 1), and how many lead the output merged by USING.
    Join {
        vars: Vec<Node>,
        left: Vec<i16>,
        right: Vec<i16>,
        merged: usize,
    },
    Values { rows: Vec<Vec<Node>> },
    Cte { name: String },
}

/// A range table entry: what it reads, the alias written for it, and the name and columns the
/// parser gave it (its eref).
#[derive(Clone, Debug)]
pub(crate) struct Rte {
    pub kind: RteKind,
    pub alias: Option<Alias>,
    pub ename: String,
    pub ecols: Vec<String>,
    pub in_from: bool,
}

/// An item of a FROM list or a join's side.
#[derive(Clone, Debug)]
pub(crate) enum Jt {
    Ref(usize),
    Join(Box<Join>),
}

#[derive(Clone, Debug)]
pub(crate) struct Join {
    pub kind: JoinKind,
    pub left: Jt,
    pub right: Jt,
    pub using: Vec<String>,
    pub quals: Option<Node>,
    pub alias: Option<Alias>,
    pub rtindex: usize,
}

/// A target list entry: its expression, its name, and whether it is projected.
#[derive(Clone, Debug)]
pub(crate) struct Target {
    pub expr: Node,
    pub name: Option<String>,
    pub junk: bool,
}

/// A sort or grouping item: the target's expression, its place in the target list (from 1), and
/// its order.
#[derive(Clone, Debug)]
pub(crate) struct Sort {
    pub expr: Node,
    pub resno: usize,
    pub desc: bool,
    pub nulls_first: bool,
}

#[derive(Clone, Debug)]
pub(crate) enum Bound {
    UnboundedPreceding,
    UnboundedFollowing,
    CurrentRow,
    Preceding(Node),
    Following(Node),
}

#[derive(Clone, Debug)]
pub(crate) struct Frame {
    pub units: &'static str,
    pub start: Bound,
    pub end: Option<Bound>,
}

/// A window clause: its name where the WINDOW clause gave one, its partition and order, its frame
/// where one was written.
#[derive(Clone, Debug)]
pub(crate) struct Window {
    pub name: Option<String>,
    pub partition: Vec<Sort>,
    pub order: Vec<Sort>,
    pub frame: Option<Frame>,
}

#[derive(Clone, Debug)]
pub(crate) enum SetOp {
    Leaf(usize),
    Op {
        op: &'static str,
        all: bool,
        left: Box<SetOp>,
        right: Box<SetOp>,
    },
}

#[derive(Clone, Debug)]
pub(crate) struct Cte {
    pub name: String,
    pub cols: Vec<String>,
    pub materialized: Option<bool>,
    pub query: Box<Query>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubKind {
    Exists,
    Any,
    All,
    Expr,
    Array,
}

/// A query as the parser leaves it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Query {
    pub ctes: Vec<Cte>,
    pub recursive: bool,
    pub rtable: Vec<Rte>,
    pub from: Vec<Jt>,
    pub quals: Option<Node>,
    pub targets: Vec<Target>,
    pub distinct: bool,
    pub group: Vec<Sort>,
    pub having: Option<Node>,
    pub windows: Vec<Window>,
    pub sort: Vec<Sort>,
    pub offset: Option<Node>,
    pub limit: Option<Node>,
    pub with_ties: bool,
    pub setop: Option<SetOp>,
}

impl Query {
    /// The projected entries.
    pub(crate) fn output(&self) -> impl Iterator<Item = &Target> {
        self.targets.iter().filter(|t| !t.junk)
    }

    /// Each projected entry's name, type and typmod.
    pub(crate) fn columns(&self) -> Vec<(String, u32, i32)> {
        self.output()
            .map(|t| {
                (
                    t.name.clone().unwrap_or_else(|| "?column?".into()),
                    t.expr.ty(),
                    t.expr.typmod(),
                )
            })
            .collect()
    }
}

/// A column a namespace item exposes: the Var a reference to it makes.
#[derive(Clone, Debug)]
pub(crate) struct NsCol {
    pub rel: u32,
    pub attno: i16,
    pub ty: u32,
    pub typmod: i32,
    pub syn: (u32, i16),
}

/// A namespace item: a relation a name or a column reference may reach.
#[derive(Clone, Debug)]
pub(crate) struct Ns {
    pub refname: String,
    pub schema: Option<String>,
    pub rel_visible: bool,
    pub cols_visible: bool,
    pub names: Vec<String>,
    pub cols: Vec<NsCol>,
}

impl Ns {
    fn var(&self, i: usize, up: u32) -> Node {
        let k = &self.cols[i];
        Node::Var {
            attno: k.attno,
            ty: k.ty,
            typmod: k.typmod,
            rel: k.rel,
            up,
            syn: k.syn,
        }
    }

    fn column(&self, c: &str, up: u32) -> Option<Node> {
        let i = self.names.iter().position(|x| x == c)?;
        Some(self.var(i, up))
    }
}

/// A WITH query a FROM item may name, and its columns.
#[derive(Clone, Debug)]
pub(crate) struct CteDef {
    pub name: String,
    pub cols: Vec<String>,
    pub types: Vec<(u32, i32)>,
}

/// A window definition as written: its name, where the WINDOW clause gave one, and its text, which
/// two OVER clauses that share are one window.
#[derive(Clone, Debug)]
struct WinDef {
    name: Option<String>,
    spec: A::WindowSpec,
    key: String,
}

/// One query level's parse state.
pub(crate) struct Scope<'p> {
    pub parent: Option<&'p Scope<'p>>,
    pub rtable: RefCell<Vec<Rte>>,
    pub names: RefCell<Vec<Ns>>,
    pub ctes: RefCell<Vec<CteDef>>,
    windows: RefCell<Vec<WinDef>>,
}

impl<'p> Scope<'p> {
    fn new(parent: Option<&'p Scope<'p>>) -> Scope<'p> {
        Scope {
            parent,
            rtable: RefCell::new(Vec::new()),
            names: RefCell::new(Vec::new()),
            ctes: RefCell::new(Vec::new()),
            windows: RefCell::new(Vec::new()),
        }
    }

    /// colNameToVar and the qualified forms: the column a reference names, at the nearest level
    /// that has it.
    pub(crate) fn column(&self, names: &[String]) -> Option<Node> {
        let mut s = Some(self);
        let mut up = 0;
        while let Some(sc) = s {
            if let Some(n) = sc.column_here(names, up) {
                return Some(n);
            }
            s = sc.parent;
            up += 1;
        }
        None
    }

    fn column_here(&self, names: &[String], up: u32) -> Option<Node> {
        let ns = self.names.borrow();
        match names {
            [c] => ns.iter().filter(|n| n.cols_visible).find_map(|n| n.column(c, up)),
            [t, c] => ns
                .iter()
                .filter(|n| n.rel_visible && n.refname == *t)
                .find_map(|n| n.column(c, up)),
            [s, t, c] => ns
                .iter()
                .filter(|n| n.rel_visible && n.refname == *t && n.schema.as_deref() == Some(s.as_str()))
                .find_map(|n| n.column(c, up)),
            _ => None,
        }
    }

    /// A WITH query by name, at the nearest level that has it.
    fn cte(&self, name: &str) -> Option<CteDef> {
        let mut s = Some(self);
        while let Some(sc) = s {
            if let Some(c) = sc.ctes.borrow().iter().find(|c| c.name == name) {
                return Some(c.clone());
            }
            s = sc.parent;
        }
        None
    }

    fn add(&self, rte: Rte) -> usize {
        let mut r = self.rtable.borrow_mut();
        r.push(rte);
        r.len()
    }

    /// The window an OVER clause names or writes: an equal one already met is the same window.
    pub(crate) fn window(&self, over: &A::WindowType) -> Result<usize> {
        let mut w = self.windows.borrow_mut();
        match over {
            A::WindowType::NamedWindow(n) => {
                let name = super::analyze::folded(n);
                w.iter()
                    .position(|d| d.name.as_deref() == Some(name.as_str()))
                    .ok_or_else(|| Error::new(format!("window \"{name}\" does not exist")))
            }
            A::WindowType::WindowSpec(spec) => {
                if spec.window_name.is_some() {
                    return Err(not_read(spec));
                }
                let key = spec.to_string();
                if let Some(i) = w.iter().position(|d| d.name.is_none() && d.key == key) {
                    return Ok(i);
                }
                w.push(WinDef {
                    name: None,
                    spec: spec.clone(),
                    key,
                });
                Ok(w.len() - 1)
            }
        }
    }
}

/// strip_implicit_coercions.
pub(crate) fn strip_implicit(n: &Node) -> &Node {
    match n {
        Node::Func {
            form: Form::Implicit,
            args,
            ..
        } => strip_implicit(&args[0]),
        Node::Relabel {
            form: Form::Implicit,
            arg,
            ..
        }
        | Node::Io {
            form: Form::Implicit,
            arg,
            ..
        }
        | Node::ArrayCoerce {
            form: Form::Implicit,
            arg,
            ..
        }
        | Node::Domain {
            form: Form::Implicit,
            arg,
            ..
        } => strip_implicit(arg),
        n => n,
    }
}

/// equal() over two expressions: a column's syntactic referent is no part of it.
pub(crate) fn same(a: &Node, b: &Node) -> bool {
    fn plain(n: &Node) -> String {
        let text = format!("{n:?}");
        // The syntactic referent prints as `syn: (r, a)`; equal() ignores it.
        let mut out = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(i) = rest.find("syn: (") {
            out.push_str(&rest[..i]);
            let after = &rest[i..];
            let end = after.find(')').map_or(after.len(), |e| e + 1);
            rest = &after[end..];
        }
        out.push_str(rest);
        out
    }
    plain(a) == plain(b)
}

/// FigureColname: the name a target list entry is given, and how strongly.
pub(crate) fn figure(cat: &Cat, e: &A::Expr, node: Option<&Node>) -> (String, u8) {
    use A::Expr as E;
    let none = ("?column?".to_string(), 0);
    match e {
        E::Nested(x) => figure(cat, x, node),
        E::Identifier(i) => (super::analyze::folded(i), 2),
        E::CompoundIdentifier(parts) => parts
            .last()
            .map_or(none, |i| (super::analyze::folded(i), 2)),
        E::CompoundFieldAccess { root, access_chain } => {
            let last = access_chain.iter().rev().find_map(|a| match a {
                A::AccessExpr::Dot(E::Identifier(i)) => Some(super::analyze::folded(i)),
                _ => None,
            });
            match last {
                Some(n) => (n, 2),
                None => figure(cat, root, None),
            }
        }
        E::Function(f) => {
            let name = f
                .name
                .0
                .last()
                .and_then(|p| p.as_ident())
                .map(super::analyze::folded)
                .unwrap_or_default();
            (name, 2)
        }
        E::Cast { expr, data_type, .. } => {
            let (n, s) = figure(cat, expr, None);
            if s > 1 {
                return (n, s);
            }
            match type_word(cat, data_type) {
                Some(t) => (t, 1),
                None => (n, s),
            }
        }
        E::TypedString(t) => type_word(cat, &t.data_type).map_or(none, |w| (w, 1)),
        E::Interval(_) => ("interval".into(), 1),
        E::Case { else_result, .. } => {
            let (n, s) = else_result
                .as_deref()
                .map_or(("?column?".to_string(), 0), |x| figure(cat, x, None));
            if s <= 1 {
                ("case".into(), 1)
            } else {
                (n, s)
            }
        }
        E::Array(_) => ("array".into(), 2),
        E::Tuple(_) => ("row".into(), 2),
        E::Exists { negated: false, .. } => ("exists".into(), 2),
        E::Subquery(_) => {
            let mut name = None;
            if let Some(n) = node {
                n.walk(&mut |x| {
                    if name.is_none() {
                        if let Node::SubLink { query, .. } = x {
                            name = query.output().next().and_then(|t| t.name.clone());
                        }
                    }
                });
            }
            name.map_or(none, |n| (n, 2))
        }
        E::Extract { .. } => ("extract".into(), 2),
        E::Substring { .. } => ("substring".into(), 2),
        E::Position { .. } => ("position".into(), 2),
        E::Overlay { .. } => ("overlay".into(), 2),
        E::AtTimeZone { .. } => ("timezone".into(), 2),
        E::Ceil { .. } => ("ceil".into(), 2),
        E::Floor { .. } => ("floor".into(), 2),
        E::Trim { trim_where, .. } => {
            use A::TrimWhereField as W;
            let n = match trim_where {
                None | Some(W::Both) => "btrim",
                Some(W::Leading) => "ltrim",
                Some(W::Trailing) => "rtrim",
            };
            (n.into(), 2)
        }
        E::Collate { expr, .. } => figure(cat, expr, node),
        _ => none,
    }
}

/// The last name of a type as the grammar names it: its pg_type name, an array's its element's.
fn type_word(cat: &Cat, dt: &A::DataType) -> Option<String> {
    let text = dt.to_string();
    let oid = crate::regs::reg_parse(crate::regs::RegKind::Type, &text, cat.release).ok()?;
    let t = cat.ty(oid)?;
    let t = if t.typtype != 'd' && cat.element(oid) != 0 {
        cat.ty(t.elem)?
    } else {
        t
    };
    Some(t.name.clone())
}

/// Whether pg_catalog holds a relation of that name, which a name alone finds before public's.
fn catalog_relation(cat: &Cat, name: &str) -> bool {
    crate::pgcatalog::built_in("pg_class", cat.release).iter().any(|r| {
        r.get("relname").cloned().flatten().as_deref() == Some(name)
            && r.get("relnamespace").cloned().flatten().as_deref() == Some("11")
    })
}

/// A schema's OID by its name.
pub(crate) fn namespace_oid(cat: &Cat, schema: &str) -> u32 {
    match schema {
        "public" => 2200,
        "pg_catalog" => 11,
        "information_schema" => crate::regs::reg_parse(crate::regs::RegKind::Namespace, schema, cat.release)
            .unwrap_or(0),
        s => cat
            .catalog
            .oids
            .of(&Object::Namespace(s.to_string()))
            .unwrap_or(0),
    }
}

/// A relation's columns: each name, type and typmod.
pub(crate) type Columns = Vec<(String, u32, i32)>;

/// A file's view analyzed, and its columns as the view names them.
pub(crate) fn view_query(cat: &Cat, v: &crate::catalog::View) -> Result<(Query, Columns)> {
    let bound = crate::grammar::bound(&v.query);
    let parsed = crate::read::image::parse_sql(bound.as_deref().unwrap_or(&v.query))?;
    let [A::Statement::Query(q)] = parsed.as_slice() else {
        return Err(Error::new(format!("the view {} is no question", v.name)));
    };
    let q = crate::grammar::query(q);
    let query = analyze(cat, &q, None, true)?;
    let mut cols = query.columns();
    for (c, n) in cols.iter_mut().zip(&v.columns) {
        c.0 = n.clone();
    }
    Ok((query, cols))
}

/// A relation's columns by its name: a file's table or view.
fn relation(cat: &Cat, key: &str) -> Result<Option<Columns>> {
    if let Some(t) = cat.catalog.tables.iter().find(|t| full(&t.name) == key) {
        let cols = super::columns_of(cat, t)
            .into_iter()
            .map(|c| (c.name, c.ty, c.typmod))
            .collect();
        return Ok(Some(cols));
    }
    if let Some(v) = cat.catalog.views.iter().find(|v| full(&v.name) == key) {
        let (_, cols) = view_query(cat, v)?;
        return Ok(Some(cols));
    }
    Ok(None)
}

/// A query analyzed at a level below `parent`, its unknown-typed outputs read as text where
/// `resolve` says so (every query but a set operation's leaves).
pub(crate) fn analyze(cat: &Cat, q: &A::Query, parent: Option<&Scope>, resolve: bool) -> Result<Query> {
    let scope = Scope::new(parent);
    Builder { cat, scope: &scope }.query(q, resolve, None)
}

/// A recursive WITH query being read: the level it is defined at, its name and its column names.
struct Recursing<'r, 'p> {
    at: &'r Scope<'p>,
    name: String,
    cols: Vec<String>,
}

struct Builder<'b, 'c, 'a> {
    cat: &'c Cat<'a>,
    scope: &'b Scope<'b>,
}

impl<'b> Builder<'b, '_, '_> {
    fn analyzer(&self) -> Analyzer<'b, '_> {
        Analyzer {
            cat: self.cat,
            columns: Vec::new(),
            value: None,
            scope: Some(self.scope),
        }
    }

    fn query(&self, q: &A::Query, resolve: bool, rec: Option<&Recursing>) -> Result<Query> {
        if q.fetch.is_some() && q.limit_clause.is_some() {
            return Err(not_read(q));
        }
        if !q.locks.is_empty()
            || q.for_clause.is_some()
            || q.settings.is_some()
            || q.format_clause.is_some()
            || !q.pipe_operators.is_empty()
        {
            return Err(not_read(q));
        }
        // A question in parentheses takes the clauses written after it.
        if let A::SetExpr::Query(inner) = q.body.as_ref() {
            let outer_bare = q.with.is_none() && q.order_by.is_none() && q.limit_clause.is_none() && q.fetch.is_none();
            if outer_bare {
                return self.query(inner, resolve, rec);
            }
            let inner_bare = inner.with.is_none()
                && inner.order_by.is_none()
                && inner.limit_clause.is_none()
                && inner.fetch.is_none();
            if inner_bare {
                let mut merged = q.clone();
                merged.body = inner.body.clone();
                return self.query(&merged, resolve, rec);
            }
            return Err(not_read(q));
        }
        let (ctes, recursive) = match &q.with {
            Some(w) => (self.with(w)?, w.recursive),
            None => (Vec::new(), false),
        };
        let mut query = match q.body.as_ref() {
            A::SetExpr::Select(s) => self.select(s, q, resolve)?,
            A::SetExpr::SetOperation { .. } => self.setops(q, rec)?,
            A::SetExpr::Values(v) => {
                if q.order_by.is_some() || q.limit_clause.is_some() || q.fetch.is_some() {
                    return Err(not_read(q));
                }
                self.values(v)?
            }
            other => return Err(not_read(other)),
        };
        query.ctes = ctes;
        query.recursive = recursive;
        Ok(query)
    }

    /// transformWithClause: each WITH query read, those before it visible to it (and itself,
    /// where it is RECURSIVE).
    fn with(&self, w: &A::With) -> Result<Vec<Cte>> {
        let mut out = Vec::new();
        for c in &w.cte_tables {
            if c.from.is_some() {
                return Err(not_read(&c.alias));
            }
            let name = super::analyze::folded(&c.alias.name);
            let cols: Vec<String> = c.alias.columns.iter().map(|x| super::analyze::folded(&x.name)).collect();
            let rec = Recursing {
                at: self.scope,
                name: name.clone(),
                cols: cols.clone(),
            };
            let sub = Scope::new(Some(self.scope));
            let b = Builder {
                cat: self.cat,
                scope: &sub,
            };
            let query = b.query(&c.query, true, w.recursive.then_some(&rec))?;
            let def = cte_def(&name, &cols, &query);
            {
                let mut ctes = self.scope.ctes.borrow_mut();
                ctes.retain(|d| d.name != name);
                ctes.push(def);
            }
            out.push(Cte {
                name,
                cols,
                materialized: c.materialized.as_ref().map(|m| matches!(m, A::CteAsMaterialized::Materialized)),
                query: Box::new(query),
            });
        }
        Ok(out)
    }

    fn select(&self, s: &A::Select, q: &A::Query, resolve: bool) -> Result<Query> {
        if s.top.is_some()
            || s.into.is_some()
            || !s.lateral_views.is_empty()
            || s.prewhere.is_some()
            || !s.connect_by.is_empty()
            || !s.cluster_by.is_empty()
            || !s.distribute_by.is_empty()
            || !s.sort_by.is_empty()
            || s.qualify.is_some()
            || s.value_table_mode.is_some()
            || s.exclude.is_some()
            || s.select_modifiers.is_some()
            || !s.optimizer_hints.is_empty()
        {
            return Err(not_read(s));
        }
        for w in &s.named_window {
            let A::NamedWindowExpr::WindowSpec(spec) = &w.1 else {
                return Err(not_read(&w.0));
            };
            let spec = spec.clone();
            self.scope.windows.borrow_mut().push(WinDef {
                name: Some(super::analyze::folded(&w.0)),
                key: spec.to_string(),
                spec,
            });
        }
        // FROM: each item's namespace in place once every item is read.
        let mut from = Vec::new();
        let mut names = Vec::new();
        for twj in &s.from {
            let (jt, _, ns) = self.table_with_joins(twj)?;
            from.push(jt);
            names.extend(ns);
        }
        *self.scope.names.borrow_mut() = names;
        let a = self.analyzer();
        let mut targets = self.targets(&s.projection)?;
        if resolve {
            for t in targets.iter_mut() {
                if t.expr.ty() == cat::UNKNOWN {
                    let n = std::mem::replace(&mut t.expr, Node::List(Vec::new()));
                    t.expr = a.coerce(n, cat::UNKNOWN, cat::TEXT, -1, Context::Implicit, Form::Implicit)?;
                }
            }
        }
        let quals = s.selection.as_ref().map(|w| a.boolean(a.expr(w)?)).transpose()?;
        let having = s.having.as_ref().map(|h| a.boolean(a.expr(h)?)).transpose()?;
        let mut sort = Vec::new();
        if let Some(o) = &q.order_by {
            let A::OrderByKind::Expressions(items) = &o.kind else {
                return Err(not_read(o));
            };
            for item in items {
                let at = self.find_target(&item.expr, &mut targets, false, false)?;
                sort.push(self.sort(&targets, at, item)?);
            }
        }
        let mut group = Vec::new();
        match &s.group_by {
            A::GroupByExpr::Expressions(exprs, modifiers) if modifiers.is_empty() => {
                for e in exprs {
                    let at = self.find_target(e, &mut targets, true, false)?;
                    if !group.iter().any(|g: &Sort| g.resno == at + 1) {
                        group.push(Sort {
                            expr: targets[at].expr.clone(),
                            resno: at + 1,
                            desc: false,
                            nulls_first: false,
                        });
                    }
                }
            }
            other => return Err(not_read(other)),
        }
        let distinct = match &s.distinct {
            None | Some(A::Distinct::All) => false,
            Some(A::Distinct::Distinct) => true,
            Some(A::Distinct::On(_)) => return Err(Error::new("DISTINCT ON is not printed as PostgreSQL prints it")),
        };
        let (offset, limit, with_ties) = self.limits(q)?;
        let windows = self.windows(&mut targets)?;
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            from,
            quals,
            targets,
            distinct,
            group,
            having,
            windows,
            sort,
            offset,
            limit,
            with_ties,
            ..Query::default()
        })
    }

    fn sort(&self, targets: &[Target], at: usize, item: &A::OrderByExpr) -> Result<Sort> {
        if item.with_fill.is_some() {
            return Err(not_read(item));
        }
        let desc = match &item.options.sort {
            None | Some(A::OrderBySort::Asc) => false,
            Some(A::OrderBySort::Desc) => true,
            Some(A::OrderBySort::Using(_)) => return Err(not_read(item)),
        };
        Ok(Sort {
            expr: targets[at].expr.clone(),
            resno: at + 1,
            desc,
            nulls_first: item.options.nulls_first.unwrap_or(desc),
        })
    }

    /// findTargetlistEntrySQL92 and SQL99: the target an ORDER BY or GROUP BY item names, one
    /// added unprojected where none is equal.
    fn find_target(&self, e: &A::Expr, targets: &mut Vec<Target>, group_by: bool, sql99: bool) -> Result<usize> {
        if !sql99 {
            if let A::Expr::Identifier(i) = e {
                let name = super::analyze::folded(i);
                let from_column = group_by && self.scope.column_here(std::slice::from_ref(&name), 0).is_some();
                if !from_column {
                    if let Some(at) = targets
                        .iter()
                        .position(|t| !t.junk && t.name.as_deref() == Some(name.as_str()))
                    {
                        return Ok(at);
                    }
                }
            }
            if let A::Expr::Value(v) = e {
                if let sqlparser::ast::Value::Number(n, _) = &v.value {
                    let pos: usize = n
                        .parse()
                        .map_err(|_| Error::new(format!("non-integer constant in ORDER BY: {n}")))?;
                    let at = targets
                        .iter()
                        .enumerate()
                        .filter(|(_, t)| !t.junk)
                        .nth(pos.wrapping_sub(1))
                        .map(|(i, _)| i)
                        .ok_or_else(|| Error::new(format!("position {pos} is not in select list")))?;
                    return Ok(at);
                }
            }
        }
        let n = self.analyzer().expr(e)?;
        if let Some(at) = targets.iter().position(|t| same(strip_implicit(&t.expr), &n)) {
            return Ok(at);
        }
        targets.push(Target {
            expr: n,
            name: None,
            junk: true,
        });
        Ok(targets.len() - 1)
    }

    /// transformLimitClause: each bound coerced to bigint.
    fn limits(&self, q: &A::Query) -> Result<(Option<Node>, Option<Node>, bool)> {
        let a = self.analyzer();
        let bigint = |e: &A::Expr| -> Result<Node> {
            let n = a.expr(e)?;
            a.coerce_to_target(n, cat::INT8, -1, Context::Assignment, Form::Implicit)
        };
        let (mut offset, mut limit, mut ties) = (None, None, false);
        match &q.limit_clause {
            None => {}
            Some(A::LimitClause::LimitOffset {
                limit: l,
                offset: o,
                limit_by,
            }) if limit_by.is_empty() => {
                limit = l.as_ref().map(&bigint).transpose()?;
                offset = o.as_ref().map(|o| bigint(&o.value)).transpose()?;
            }
            Some(other) => return Err(not_read(other)),
        }
        if let Some(f) = &q.fetch {
            if f.percent {
                return Err(not_read(f));
            }
            let one = A::Expr::Value(sqlparser::ast::Value::Number("1".into(), false).into());
            limit = Some(bigint(f.quantity.as_ref().unwrap_or(&one))?);
            ties = f.with_ties;
        }
        Ok((offset, limit, ties))
    }

    /// transformWindowDefinitions: each window met, its partition and order read as expressions.
    fn windows(&self, targets: &mut Vec<Target>) -> Result<Vec<Window>> {
        let defs = self.scope.windows.borrow().clone();
        let mut out = Vec::new();
        for d in defs {
            let mut partition = Vec::new();
            for e in &d.spec.partition_by {
                let at = self.find_target(e, targets, false, true)?;
                partition.push(Sort {
                    expr: targets[at].expr.clone(),
                    resno: at + 1,
                    desc: false,
                    nulls_first: false,
                });
            }
            let mut order = Vec::new();
            for item in &d.spec.order_by {
                let at = self.find_target(&item.expr, targets, false, true)?;
                order.push(self.sort(targets, at, item)?);
            }
            let frame = d.spec.window_frame.as_ref().map(|f| self.frame(f)).transpose()?;
            out.push(Window {
                name: d.name.clone(),
                partition,
                order,
                frame,
            });
        }
        Ok(out)
    }

    fn frame(&self, f: &A::WindowFrame) -> Result<Frame> {
        let units = match f.units {
            A::WindowFrameUnits::Rows => "ROWS",
            A::WindowFrameUnits::Range => "RANGE",
            A::WindowFrameUnits::Groups => "GROUPS",
        };
        let a = self.analyzer();
        let bound = |b: &A::WindowFrameBound| -> Result<Bound> {
            let offset = |e: &A::Expr| -> Result<Node> {
                if units == "RANGE" {
                    return Err(Error::new("a RANGE frame with an offset is not printed as PostgreSQL prints it"));
                }
                let n = a.expr(e)?;
                a.coerce_to_target(n, cat::INT8, -1, Context::Assignment, Form::Implicit)
            };
            Ok(match b {
                A::WindowFrameBound::CurrentRow => Bound::CurrentRow,
                A::WindowFrameBound::Preceding(None) => Bound::UnboundedPreceding,
                A::WindowFrameBound::Following(None) => Bound::UnboundedFollowing,
                A::WindowFrameBound::Preceding(Some(e)) => Bound::Preceding(offset(e)?),
                A::WindowFrameBound::Following(Some(e)) => Bound::Following(offset(e)?),
            })
        };
        Ok(Frame {
            units,
            start: bound(&f.start_bound)?,
            end: f.end_bound.as_ref().map(bound).transpose()?,
        })
    }

    /// transformTargetList: each item, a star expanded into the columns it reaches.
    fn targets(&self, items: &[A::SelectItem]) -> Result<Vec<Target>> {
        let a = self.analyzer();
        let mut out = Vec::new();
        for item in items {
            match item {
                A::SelectItem::UnnamedExpr(e) => {
                    let n = a.expr(e)?;
                    let (name, _) = figure(self.cat, e, Some(&n));
                    out.push(Target {
                        expr: n,
                        name: Some(name),
                        junk: false,
                    });
                }
                A::SelectItem::ExprWithAlias { expr, alias } => {
                    let n = a.expr(expr)?;
                    out.push(Target {
                        expr: n,
                        name: Some(super::analyze::folded(alias)),
                        junk: false,
                    });
                }
                A::SelectItem::Wildcard(opts) => {
                    plain_star(opts)?;
                    for ns in self.scope.names.borrow().iter().filter(|n| n.cols_visible) {
                        expand(ns, &mut out);
                    }
                }
                A::SelectItem::QualifiedWildcard(A::SelectItemQualifiedWildcardKind::ObjectName(o), opts) => {
                    plain_star(opts)?;
                    let parts: Vec<String> = o.0.iter().filter_map(|p| p.as_ident().map(super::analyze::folded)).collect();
                    let names = self.scope.names.borrow();
                    let ns = match parts.as_slice() {
                        [t] => names.iter().find(|n| n.rel_visible && n.refname == *t),
                        [s, t] => names
                            .iter()
                            .find(|n| n.rel_visible && n.refname == *t && n.schema.as_deref() == Some(s.as_str())),
                        _ => None,
                    }
                    .ok_or_else(|| Error::new(format!("missing FROM-clause entry for table \"{o}\"")))?;
                    expand(ns, &mut out);
                }
                other => return Err(not_read(other)),
            }
        }
        Ok(out)
    }

    fn table_with_joins(&self, twj: &A::TableWithJoins) -> Result<(Jt, Ns, Vec<Ns>)> {
        let (mut jt, mut top, mut ns) = self.range_item(&twj.relation)?;
        for j in &twj.joins {
            if j.global {
                return Err(not_read(&j.relation));
            }
            let (rjt, rtop, rns) = self.range_item(&j.relation)?;
            (jt, top, ns) = self.join(jt, top, ns, rjt, rtop, rns, &j.join_operator)?;
        }
        Ok((jt, top, ns))
    }

    /// transformFromClauseItem.
    fn range_item(&self, t: &A::TableFactor) -> Result<(Jt, Ns, Vec<Ns>)> {
        match t {
            A::TableFactor::Table {
                name,
                alias,
                args: None,
                with_hints,
                version: None,
                with_ordinality: false,
                partitions,
                json_path: None,
                sample: None,
                index_hints,
            } if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() => {
                let parts: Vec<String> = name.0.iter().filter_map(|p| p.as_ident().map(super::analyze::folded)).collect();
                let alias = alias.as_ref().map(table_alias).transpose()?;
                if let [n] = parts.as_slice() {
                    if let Some(def) = self.scope.cte(n) {
                        return Ok(self.cte_item(def, alias));
                    }
                }
                let (schema, rel) = match parts.as_slice() {
                    [n] => ("public".to_string(), n.clone()),
                    [s, n] => (s.clone(), n.clone()),
                    _ => return Err(not_read(name)),
                };
                if schema == "public" && parts.len() == 1 && catalog_relation(self.cat, &rel) {
                    return Err(not_read(name));
                }
                let key = format!("{schema}.{rel}");
                let cols = relation(self.cat, &key)?
                    .ok_or_else(|| Error::new(format!("relation \"{name}\" does not exist")))?;
                let names = renamed(cols.iter().map(|c| c.0.clone()).collect(), alias.as_ref());
                let ename = alias.as_ref().map_or_else(|| rel.clone(), |a| a.name.clone());
                let rtindex = self.scope.add(Rte {
                    kind: RteKind::Relation {
                        name: rel,
                        namespace: namespace_oid(self.cat, &schema),
                        columns: cols.iter().map(|c| c.0.clone()).collect(),
                    },
                    alias: alias.clone(),
                    ename: ename.clone(),
                    ecols: names.clone(),
                    in_from: true,
                });
                let ns = Ns {
                    refname: ename,
                    schema: alias.is_none().then_some(schema),
                    rel_visible: true,
                    cols_visible: true,
                    names,
                    cols: plain_cols(rtindex, &cols),
                };
                Ok((Jt::Ref(rtindex), ns.clone(), vec![ns]))
            }
            A::TableFactor::Derived {
                lateral: false,
                subquery,
                alias,
                sample: None,
            } => {
                let alias = alias.as_ref().map(table_alias).transpose()?;
                let sub = analyze(self.cat, subquery, Some(self.scope), true)?;
                let cols = sub.columns();
                let names = renamed(cols.iter().map(|c| c.0.clone()).collect(), alias.as_ref());
                let ename = alias.as_ref().map_or_else(|| "unnamed_subquery".to_string(), |a| a.name.clone());
                let rtindex = self.scope.add(Rte {
                    kind: RteKind::Subquery(Box::new(sub)),
                    alias: alias.clone(),
                    ename: ename.clone(),
                    ecols: names.clone(),
                    in_from: true,
                });
                let ns = Ns {
                    refname: ename,
                    schema: None,
                    rel_visible: alias.is_some(),
                    cols_visible: true,
                    names,
                    cols: plain_cols(rtindex, &cols),
                };
                Ok((Jt::Ref(rtindex), ns.clone(), vec![ns]))
            }
            A::TableFactor::NestedJoin {
                table_with_joins,
                alias,
            } => {
                let (jt, top, ns) = self.table_with_joins(table_with_joins)?;
                let Some(alias) = alias else {
                    return Ok((jt, top, ns));
                };
                let Jt::Join(mut j) = jt else {
                    return Err(not_read(t));
                };
                let alias = table_alias(alias)?;
                let names = renamed(top.names.clone(), Some(&alias));
                {
                    let mut rt = self.scope.rtable.borrow_mut();
                    let rte = &mut rt[j.rtindex - 1];
                    rte.alias = Some(alias.clone());
                    rte.ename = alias.name.clone();
                    rte.ecols = names.clone();
                }
                let cols = top
                    .cols
                    .iter()
                    .enumerate()
                    .map(|(k, c)| NsCol {
                        syn: (j.rtindex as u32, k as i16 + 1),
                        ..c.clone()
                    })
                    .collect();
                j.alias = Some(alias.clone());
                let ns = Ns {
                    refname: alias.name,
                    schema: None,
                    rel_visible: true,
                    cols_visible: true,
                    names,
                    cols,
                };
                Ok((Jt::Join(j), ns.clone(), vec![ns]))
            }
            other => Err(not_read(other)),
        }
    }

    fn cte_item(&self, def: CteDef, alias: Option<Alias>) -> (Jt, Ns, Vec<Ns>) {
        let names = renamed(def.cols.clone(), alias.as_ref());
        let ename = alias.as_ref().map_or_else(|| def.name.clone(), |a| a.name.clone());
        let rtindex = self.scope.add(Rte {
            kind: RteKind::Cte { name: def.name.clone() },
            alias,
            ename: ename.clone(),
            ecols: names.clone(),
            in_from: true,
        });
        let cols: Vec<(String, u32, i32)> = def
            .cols
            .iter()
            .zip(&def.types)
            .map(|(n, (t, m))| (n.clone(), *t, *m))
            .collect();
        let ns = Ns {
            refname: ename,
            schema: None,
            rel_visible: true,
            cols_visible: true,
            names,
            cols: plain_cols(rtindex, &cols),
        };
        (Jt::Ref(rtindex), ns.clone(), vec![ns])
    }

    /// A join of two FROM items, its columns those merged by USING first, then the left's, then
    /// the right's (transformFromClauseItem's JoinExpr).
    #[allow(clippy::too_many_arguments)]
    fn join(&self, l: Jt, lt: Ns, lns: Vec<Ns>, r: Jt, rt: Ns, rns: Vec<Ns>, op: &A::JoinOperator) -> Result<(Jt, Ns, Vec<Ns>)> {
        use A::JoinOperator as O;
        let (kind, c) = match op {
            O::Join(c) | O::Inner(c) => (JoinKind::Inner, c),
            O::Left(c) | O::LeftOuter(c) => (JoinKind::Left, c),
            O::Right(c) | O::RightOuter(c) => (JoinKind::Right, c),
            O::FullOuter(c) => (JoinKind::Full, c),
            O::CrossJoin(c) => (JoinKind::Inner, c),
            _ => return Err(Error::new("a join of that kind is not printed as PostgreSQL prints it")),
        };
        let using: Vec<String> = match c {
            A::JoinConstraint::Using(cols) => cols
                .iter()
                .map(|o| o.0.last().and_then(|p| p.as_ident()).map(super::analyze::folded).unwrap_or_default())
                .collect(),
            A::JoinConstraint::Natural => lt.names.iter().filter(|n| rt.names.contains(n)).cloned().collect(),
            _ => Vec::new(),
        };
        let a = self.analyzer();
        let rtindex = self.scope.rtable.borrow().len() + 1;
        let (mut names, mut cols, mut vars, mut lcols, mut rcols) = (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let (mut lused, mut rused) = (Vec::new(), Vec::new());
        for (k, u) in using.iter().enumerate() {
            let li = lt
                .names
                .iter()
                .position(|n| n == u)
                .ok_or_else(|| Error::new(format!("column \"{u}\" specified in USING clause does not exist in left table")))?;
            let ri = rt
                .names
                .iter()
                .position(|n| n == u)
                .ok_or_else(|| Error::new(format!("column \"{u}\" specified in USING clause does not exist in right table")))?;
            lused.push(li);
            rused.push(ri);
            let (lv, rv) = (lt.var(li, 0), rt.var(ri, 0));
            let (res, which) = merged(&a, kind, lv, rv)?;
            let col = match which {
                Side::Left => lt.cols[li].clone(),
                Side::Right => rt.cols[ri].clone(),
                Side::Neither => NsCol {
                    rel: rtindex as u32,
                    attno: k as i16 + 1,
                    ty: res.ty(),
                    typmod: res.typmod(),
                    syn: (rtindex as u32, k as i16 + 1),
                },
            };
            names.push(u.clone());
            cols.push(col);
            vars.push(res);
            lcols.push(li as i16 + 1);
            rcols.push(ri as i16 + 1);
        }
        for (i, n) in lt.names.iter().enumerate() {
            if !lused.contains(&i) {
                names.push(n.clone());
                cols.push(lt.cols[i].clone());
                vars.push(lt.var(i, 0));
                lcols.push(i as i16 + 1);
            }
        }
        for (i, n) in rt.names.iter().enumerate() {
            if !rused.contains(&i) {
                names.push(n.clone());
                cols.push(rt.cols[i].clone());
                vars.push(rt.var(i, 0));
                rcols.push(i as i16 + 1);
            }
        }
        // The ON condition sees the two sides alone, and the levels above.
        let mut both: Vec<Ns> = lns.clone();
        both.extend(rns.iter().cloned());
        let quals = match c {
            A::JoinConstraint::On(e) => {
                let before = std::mem::replace(&mut *self.scope.names.borrow_mut(), both.clone());
                let q = a.expr(e).and_then(|n| a.boolean(n));
                *self.scope.names.borrow_mut() = before;
                Some(q?)
            }
            _ => None,
        };
        let added = self.scope.add(Rte {
            kind: RteKind::Join {
                vars,
                left: lcols,
                right: rcols,
                merged: using.len(),
            },
            alias: None,
            ename: "unnamed_join".into(),
            ecols: names.clone(),
            in_from: true,
        });
        debug_assert_eq!(added, rtindex);
        let top = Ns {
            refname: "unnamed_join".into(),
            schema: None,
            rel_visible: false,
            cols_visible: true,
            names,
            cols,
        };
        let mut ns: Vec<Ns> = both
            .into_iter()
            .map(|mut n| {
                n.cols_visible = false;
                n
            })
            .collect();
        ns.push(top.clone());
        let j = Join {
            kind,
            left: l,
            right: r,
            using,
            quals,
            alias: None,
            rtindex,
        };
        Ok((Jt::Join(Box::new(j)), top, ns))
    }

    /// transformSetOperationStmt: each leaf a subquery of the range table, each column's type the
    /// common type of its leaves', the output named as the leftmost leaf names it.
    fn setops(&self, q: &A::Query, rec: Option<&Recursing>) -> Result<Query> {
        let (tree, cols) = self.setop_tree(&q.body, rec, true)?;
        let leftmost = {
            let rt = self.scope.rtable.borrow();
            match &rt[0].kind {
                RteKind::Subquery(s) => s.output().map(|t| t.name.clone()).collect::<Vec<_>>(),
                _ => Vec::new(),
            }
        };
        let targets: Vec<Target> = cols
            .iter()
            .enumerate()
            .map(|(i, (ty, typmod))| Target {
                expr: Node::Var {
                    attno: i as i16 + 1,
                    ty: *ty,
                    typmod: *typmod,
                    rel: 1,
                    up: 0,
                    syn: (1, i as i16 + 1),
                },
                name: leftmost.get(i).cloned().flatten(),
                junk: false,
            })
            .collect();
        let mut sort = Vec::new();
        if let Some(o) = &q.order_by {
            let A::OrderByKind::Expressions(items) = &o.kind else {
                return Err(not_read(o));
            };
            for item in items {
                let at = match &item.expr {
                    A::Expr::Value(v) => match &v.value {
                        sqlparser::ast::Value::Number(n, _) => n.parse::<usize>().ok().and_then(|p| p.checked_sub(1)),
                        _ => None,
                    },
                    A::Expr::Identifier(i) => {
                        let n = super::analyze::folded(i);
                        targets.iter().position(|t| t.name.as_deref() == Some(n.as_str()))
                    }
                    _ => None,
                }
                .filter(|&p| p < targets.len())
                .ok_or_else(|| not_read(&item.expr))?;
                sort.push(self.sort(&targets, at, item)?);
            }
        }
        let (offset, limit, with_ties) = self.limits(q)?;
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            targets,
            sort,
            offset,
            limit,
            with_ties,
            setop: Some(tree),
            ..Query::default()
        })
    }

    fn setop_tree(&self, e: &A::SetExpr, rec: Option<&Recursing>, top: bool) -> Result<(SetOp, Vec<(u32, i32)>)> {
        match e {
            A::SetExpr::SetOperation {
                left,
                op,
                set_quantifier,
                right,
            } => {
                let op = match op {
                    A::SetOperator::Union => "UNION",
                    A::SetOperator::Intersect => "INTERSECT",
                    A::SetOperator::Except => "EXCEPT",
                    A::SetOperator::Minus => return Err(not_read(e)),
                };
                let all = match set_quantifier {
                    A::SetQuantifier::All => true,
                    A::SetQuantifier::Distinct | A::SetQuantifier::None => false,
                    _ => return Err(not_read(e)),
                };
                let (l, lcols) = self.setop_tree(left, None, false)?;
                if let (true, Some(r)) = (top, rec) {
                    // The non-recursive term's columns let the recursive one name the query.
                    let names: Vec<String> = if r.cols.is_empty() {
                        self.leaf_names(&l)
                    } else {
                        r.cols.clone()
                    };
                    let mut ctes = r.at.ctes.borrow_mut();
                    ctes.retain(|d| d.name != r.name);
                    ctes.push(CteDef {
                        name: r.name.clone(),
                        cols: names,
                        types: lcols.clone(),
                    });
                }
                let (rr, rcols) = self.setop_tree(right, None, false)?;
                if lcols.len() != rcols.len() {
                    return Err(Error::new(format!("each {op} query must have the same number of columns")));
                }
                let a = self.analyzer();
                let mut cols = Vec::new();
                for i in 0..lcols.len() {
                    let ty = a.common_type_of(&[lcols[i].0, rcols[i].0])?;
                    let typmod = if lcols[i].0 == rcols[i].0 && lcols[i].1 == rcols[i].1 {
                        lcols[i].1
                    } else {
                        -1
                    };
                    for side in [&l, &rr] {
                        self.coerce_unknown_leaf(side, i, ty)?;
                    }
                    cols.push((ty, typmod));
                }
                Ok((
                    SetOp::Op {
                        op,
                        all,
                        left: Box::new(l),
                        right: Box::new(rr),
                    },
                    cols,
                ))
            }
            // A set operation in parentheses with no clauses of its own is part of the tree.
            A::SetExpr::Query(q)
                if q.with.is_none()
                    && q.order_by.is_none()
                    && q.limit_clause.is_none()
                    && q.fetch.is_none()
                    && matches!(q.body.as_ref(), A::SetExpr::SetOperation { .. } | A::SetExpr::Query(_)) =>
            {
                self.setop_tree(&q.body, rec, top)
            }
            A::SetExpr::Select(_) | A::SetExpr::Values(_) | A::SetExpr::Query(_) => {
                let leaf = match e {
                    A::SetExpr::Query(q) => (**q).clone(),
                    other => A::Query {
                        with: None,
                        body: Box::new(other.clone()),
                        order_by: None,
                        limit_clause: None,
                        fetch: None,
                        locks: Vec::new(),
                        for_clause: None,
                        settings: None,
                        format_clause: None,
                        pipe_operators: Vec::new(),
                    },
                };
                let sub = analyze(self.cat, &leaf, Some(self.scope), false)?;
                let cols: Vec<(u32, i32)> = sub.columns().into_iter().map(|c| (c.1, c.2)).collect();
                let names: Vec<String> = sub.columns().into_iter().map(|c| c.0).collect();
                let n = self.scope.rtable.borrow().len() + 1;
                let rtindex = self.scope.add(Rte {
                    kind: RteKind::Subquery(Box::new(sub)),
                    alias: None,
                    ename: format!("*SELECT* {n}"),
                    ecols: names,
                    in_from: false,
                });
                Ok((SetOp::Leaf(rtindex), cols))
            }
            other => Err(not_read(other)),
        }
    }

    fn leaf_names(&self, t: &SetOp) -> Vec<String> {
        match t {
            SetOp::Leaf(i) => self.scope.rtable.borrow()[*i - 1].ecols.clone(),
            SetOp::Op { left, .. } => self.leaf_names(left),
        }
    }

    /// An unknown-typed constant a leaf outputs takes the column's type.
    fn coerce_unknown_leaf(&self, side: &SetOp, i: usize, ty: u32) -> Result<()> {
        let SetOp::Leaf(at) = side else {
            return Ok(());
        };
        let mut rt = self.scope.rtable.borrow_mut();
        let RteKind::Subquery(sub) = &mut rt[*at - 1].kind else {
            return Ok(());
        };
        let Some(t) = sub.targets.iter_mut().filter(|t| !t.junk).nth(i) else {
            return Ok(());
        };
        if t.expr.ty() == cat::UNKNOWN && matches!(t.expr, Node::Const { .. }) {
            let a = self.analyzer();
            let n = std::mem::replace(&mut t.expr, Node::List(Vec::new()));
            t.expr = a.to_common(n, ty)?;
        }
        Ok(())
    }

    /// transformValuesClause: a VALUES list read as a query over one VALUES relation.
    fn values(&self, v: &A::Values) -> Result<Query> {
        let a = self.analyzer();
        let mut rows: Vec<Vec<Node>> = Vec::new();
        for r in &v.rows {
            rows.push(r.content.iter().map(|x| a.expr(x)).collect::<Result<Vec<_>>>()?);
        }
        let width = rows.first().map_or(0, Vec::len);
        if rows.iter().any(|r| r.len() != width) {
            return Err(Error::new("VALUES lists must all be the same length"));
        }
        let mut types = Vec::new();
        for i in 0..width {
            let col: Vec<Node> = rows.iter().map(|r| r[i].clone()).collect();
            let ty = a.common_type(&col)?;
            for r in rows.iter_mut() {
                let n = std::mem::replace(&mut r[i], Node::List(Vec::new()));
                r[i] = a.to_common(n, ty)?;
            }
            let typmod = {
                let m = rows[0][i].typmod();
                if rows.iter().all(|r| r[i].ty() == ty && r[i].typmod() == m) {
                    m
                } else {
                    -1
                }
            };
            types.push((ty, typmod));
        }
        let names: Vec<String> = (1..=width).map(|i| format!("column{i}")).collect();
        let rtindex = self.scope.add(Rte {
            kind: RteKind::Values { rows },
            alias: None,
            ename: "*VALUES*".into(),
            ecols: names.clone(),
            in_from: true,
        });
        let targets = types
            .iter()
            .enumerate()
            .map(|(i, (ty, typmod))| Target {
                expr: Node::Var {
                    attno: i as i16 + 1,
                    ty: *ty,
                    typmod: *typmod,
                    rel: rtindex as u32,
                    up: 0,
                    syn: (rtindex as u32, i as i16 + 1),
                },
                name: Some(names[i].clone()),
                junk: false,
            })
            .collect();
        Ok(Query {
            rtable: self.scope.rtable.borrow().clone(),
            from: vec![Jt::Ref(rtindex)],
            targets,
            ..Query::default()
        })
    }
}

fn cte_def(name: &str, cols: &[String], q: &Query) -> CteDef {
    let out = q.columns();
    let names = out
        .iter()
        .enumerate()
        .map(|(i, c)| cols.get(i).cloned().unwrap_or_else(|| c.0.clone()))
        .collect();
    CteDef {
        name: name.to_string(),
        cols: names,
        types: out.iter().map(|c| (c.1, c.2)).collect(),
    }
}

fn plain_star(o: &A::WildcardAdditionalOptions) -> Result<()> {
    if o.opt_ilike.is_some()
        || o.opt_exclude.is_some()
        || o.opt_except.is_some()
        || o.opt_replace.is_some()
        || o.opt_rename.is_some()
        || o.opt_alias.is_some()
    {
        return Err(Error::new("a star with options is not printed as PostgreSQL prints it"));
    }
    Ok(())
}

/// A namespace item's columns as a star expands them.
fn expand(ns: &Ns, out: &mut Vec<Target>) {
    for (i, n) in ns.names.iter().enumerate() {
        out.push(Target {
            expr: ns.var(i, 0),
            name: Some(n.clone()),
            junk: false,
        });
    }
}

fn plain_cols(rtindex: usize, cols: &[(String, u32, i32)]) -> Vec<NsCol> {
    cols.iter()
        .enumerate()
        .map(|(i, c)| NsCol {
            rel: rtindex as u32,
            attno: i as i16 + 1,
            ty: c.1,
            typmod: c.2,
            syn: (rtindex as u32, i as i16 + 1),
        })
        .collect()
}

/// A relation's column names with an alias's names in place of the first.
fn renamed(mut names: Vec<String>, alias: Option<&Alias>) -> Vec<String> {
    if let Some(a) = alias {
        for (n, c) in names.iter_mut().zip(&a.cols) {
            *n = c.clone();
        }
    }
    names
}

fn table_alias(a: &A::TableAlias) -> Result<Alias> {
    if a.at.is_some() || a.columns.iter().any(|c| c.data_type.is_some()) {
        return Err(not_read(a));
    }
    Ok(Alias {
        name: super::analyze::folded(&a.name),
        cols: a.columns.iter().map(|c| super::analyze::folded(&c.name)).collect(),
    })
}

enum Side {
    Left,
    Right,
    Neither,
}

/// buildMergedJoinVar: a column merged by USING, coerced to the two sides' common type.
fn merged(a: &Analyzer, kind: JoinKind, l: Node, r: Node) -> Result<(Node, Side)> {
    let ty = a.common_type_of(&[l.ty(), r.ty()])?;
    let typmod = if l.typmod() == r.typmod() { l.typmod() } else { -1 };
    let side = |n: Node| -> Result<(Node, bool)> {
        if n.ty() != ty {
            let from = n.ty();
            Ok((a.coerce(n, from, ty, typmod, Context::Implicit, Form::Implicit)?, false))
        } else if n.typmod() != typmod {
            Ok((
                Node::Relabel {
                    arg: Box::new(n),
                    ty,
                    typmod,
                    form: Form::Implicit,
                },
                false,
            ))
        } else {
            Ok((n, true))
        }
    };
    let (ln, lplain) = side(l)?;
    let (rn, rplain) = side(r)?;
    Ok(match kind {
        JoinKind::Inner => {
            if lplain {
                (ln, Side::Left)
            } else if rplain {
                (rn, Side::Right)
            } else {
                (ln, Side::Neither)
            }
        }
        JoinKind::Left => (ln, if lplain { Side::Left } else { Side::Neither }),
        JoinKind::Right => (rn, if rplain { Side::Right } else { Side::Neither }),
        JoinKind::Full => (
            Node::Coalesce {
                args: vec![ln, rn],
                ty,
            },
            Side::Neither,
        ),
    })
}

impl Analyzer<'_, '_> {
    /// A function called inside a query: an aggregate or a window function as the parser makes
    /// one; None where the call is no such thing, for the plain reading.
    pub(crate) fn query_function(&self, f: &A::Function, e: &A::Expr) -> Result<Option<Node>> {
        let Some(scope) = self.scope else {
            return Ok(None);
        };
        let parts: Vec<String> = f.name.0.iter().filter_map(|p| p.as_ident().map(super::analyze::folded)).collect();
        let (schema, name) = match parts.as_slice() {
            [n] => (None, n.clone()),
            [s, n] => (Some(s.clone()), n.clone()),
            _ => return Ok(None),
        };
        if let A::FunctionArguments::Subquery(q) = &f.args {
            if schema.is_none() && name == "array" {
                let sub = analyze(self.cat, q, Some(scope), true)?;
                let first = sub.output().next().map_or(cat::TEXT, |t| t.expr.ty());
                return Ok(Some(Node::SubLink {
                    kind: SubKind::Array,
                    test: None,
                    query: Box::new(sub),
                    ty: self.cat.array_of(first),
                    typmod: -1,
                }));
            }
            return Err(not_read(e));
        }
        let A::FunctionArguments::List(l) = &f.args else {
            return Ok(None);
        };
        if schema.is_none()
            && matches!(
                name.as_str(),
                "coalesce" | "greatest" | "least" | "nullif" | "current_time" | "current_timestamp" | "localtime" | "localtimestamp"
            )
        {
            return Ok(None);
        }
        let mut star = false;
        let mut exprs = Vec::new();
        for arg in &l.args {
            match arg {
                A::FunctionArg::Unnamed(A::FunctionArgExpr::Expr(x)) => exprs.push(x),
                A::FunctionArg::Unnamed(A::FunctionArgExpr::Wildcard) if l.args.len() == 1 => star = true,
                _ => return Err(not_read(e)),
            }
        }
        let distinct = match l.duplicate_treatment {
            None | Some(A::DuplicateTreatment::All) => false,
            Some(A::DuplicateTreatment::Distinct) => true,
        };
        let mut order: &[A::OrderByExpr] = &[];
        for c in &l.clauses {
            match c {
                A::FunctionArgumentClause::OrderBy(o) => order = o,
                _ => return Err(not_read(e)),
            }
        }
        if !f.within_group.is_empty() || f.null_treatment.is_some() || !matches!(f.parameters, A::FunctionArguments::None) {
            return Err(not_read(e));
        }
        let featured = star || distinct || !order.is_empty() || f.filter.is_some() || f.over.is_some();
        let nodes = exprs.iter().map(|x| self.expr(x)).collect::<Result<Vec<_>>>()?;
        let schema = schema
            .map(|s| crate::regs::reg_parse(crate::regs::RegKind::Namespace, &s, self.cat.release))
            .transpose()?;
        let n = self.func_call(&name, schema, nodes, e)?;
        let Node::Func {
            oid,
            args,
            ty,
            form: Form::Call,
            variadic,
        } = n
        else {
            if featured {
                return Err(not_read(e));
            }
            return Ok(Some(n));
        };
        let kind = self.cat.proc(oid).map_or('f', |p| p.kind);
        if kind != 'a' && kind != 'w' {
            if featured {
                return Err(Error::new(format!(
                    "OVER, FILTER, DISTINCT or ORDER BY specified, but {name} is not an aggregate function"
                )));
            }
            return Ok(Some(Node::Func {
                oid,
                args,
                ty,
                form: Form::Call,
                variadic,
            }));
        }
        let filter = f
            .filter
            .as_ref()
            .map(|x| self.expr(x).and_then(|n| self.boolean(n)))
            .transpose()?
            .map(Box::new);
        if let Some(over) = &f.over {
            if distinct || !order.is_empty() {
                return Err(Error::new("DISTINCT or ORDER BY is not implemented for window functions"));
            }
            let win = scope.window(over)?;
            return Ok(Some(Node::Window {
                oid,
                args,
                star,
                filter,
                win,
                ty,
            }));
        }
        if kind == 'w' {
            return Err(Error::new(format!("window function {name} requires an OVER clause")));
        }
        let mut sorts = Vec::new();
        for item in order {
            let x = self.expr(&item.expr)?;
            let x = args.iter().find(|a| same(strip_implicit(a), &x)).cloned().unwrap_or(x);
            let desc = match &item.options.sort {
                None | Some(A::OrderBySort::Asc) => false,
                Some(A::OrderBySort::Desc) => true,
                Some(A::OrderBySort::Using(_)) => return Err(not_read(e)),
            };
            sorts.push(Sort {
                expr: x,
                resno: 0,
                desc,
                nulls_first: item.options.nulls_first.unwrap_or(desc),
            });
        }
        Ok(Some(Node::Agg {
            oid,
            args,
            order: sorts,
            distinct,
            star,
            filter,
            variadic,
            ty,
        }))
    }

    /// transformSubLink: a subquery as an expression.
    pub(crate) fn sublink(&self, e: &A::Expr) -> Result<Node> {
        let Some(scope) = self.scope else {
            return Err(Error::new("cannot use subquery here"));
        };
        let sub = |q: &A::Query| analyze(self.cat, q, Some(scope), true);
        let first = |q: &Query| q.output().next().map_or((cat::TEXT, -1), |t| (t.expr.ty(), t.expr.typmod()));
        let compared = |left: &A::Expr, op: &str, q: &A::Query, kind: SubKind| -> Result<Node> {
            let l = self.expr(left)?;
            let query = sub(q)?;
            let (ty, typmod) = first(&query);
            let test = self.op(op, None, Some(l), Node::CaseTest { ty, typmod })?;
            let Node::Op { oid, mut args, .. } = test else {
                return Err(not_read(e));
            };
            let right = args.pop().map_or(ty, |a| a.ty());
            let left = args.pop().ok_or_else(|| not_read(e))?;
            Ok(Node::SubLink {
                kind,
                test: Some((Box::new(left), oid, right)),
                query: Box::new(query),
                ty: cat::BOOL,
                typmod: -1,
            })
        };
        let not = |n: Node| Node::Bool {
            op: BoolOp::Not,
            args: vec![n],
        };
        Ok(match e {
            A::Expr::Subquery(q) => {
                let query = sub(q)?;
                let (ty, typmod) = first(&query);
                Node::SubLink {
                    kind: SubKind::Expr,
                    test: None,
                    query: Box::new(query),
                    ty,
                    typmod,
                }
            }
            A::Expr::Exists { subquery, negated } => {
                let n = Node::SubLink {
                    kind: SubKind::Exists,
                    test: None,
                    query: Box::new(sub(subquery)?),
                    ty: cat::BOOL,
                    typmod: -1,
                };
                if *negated {
                    not(n)
                } else {
                    n
                }
            }
            A::Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => {
                let n = compared(expr, "=", subquery, SubKind::Any)?;
                if *negated {
                    not(n)
                } else {
                    n
                }
            }
            A::Expr::AnyOp {
                left,
                compare_op,
                right,
                ..
            }
            | A::Expr::AllOp {
                left,
                compare_op,
                right,
            } => {
                let A::Expr::Subquery(q) = right.as_ref() else {
                    return Err(not_read(e));
                };
                let name = super::analyze::binary_name(compare_op).ok_or_else(|| not_read(e))?;
                let kind = if matches!(e, A::Expr::AnyOp { .. }) { SubKind::Any } else { SubKind::All };
                compared(left, &name, q, kind)?
            }
            other => return Err(not_read(other)),
        })
    }
}
