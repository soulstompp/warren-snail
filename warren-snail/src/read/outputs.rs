//! A subquery's outputs nothing reads, left unmade as PostgreSQL 18.6's planner leaves them: a
//! FROM subquery's, each arm's of a FROM subquery of UNION ALLs whose arms' columns are of the
//! union's types, and a WITH query's where it is read in place (once and not MATERIALIZED, or NOT
//! MATERIALIZED, calling nothing volatile). An output is made all the same where its query returns
//! each row once (DISTINCT) or sorts, groups or keeps a run by it, where it makes a set, calls a
//! window that reads its arguments, or calls what may be volatile, and where what reads the
//! subquery reads its whole row or a star, or meets it by NATURAL on a name it shares. One left
//! unmade is read as `CASE WHEN false THEN … END`: typed as it is, and never evaluated; each
//! aggregate it calls folds no row, `FILTER (WHERE false)`, so that its query groups as before.

use super::pipeline;
use crate::catalog::{ident, Catalog};
use sqlparser::ast::{
    CaseWhen, CteAsMaterialized, Distinct, Expr, FunctionArgExpr, FunctionArguments, GroupByExpr,
    Ident, JoinConstraint, JoinOperator, ObjectName, OrderByKind, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, SetOperator, SetQuantifier, TableFactor,
    TableWithJoins, Value, Visit, VisitMut, Visitor, VisitorMut,
};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;

/// Every output nothing reads of the subqueries a statement holds, written to be left unmade.
pub(super) fn dropped(q: &mut Query, catalog: &Catalog) {
    if holds_subqueries(q) {
        let _ = VisitMut::visit(q, &mut Dropping { catalog });
    }
}

/// Whether a statement holds a FROM subquery or a WITH.
pub(super) fn holds_subqueries(q: &Query) -> bool {
    struct Holds;
    impl Visitor for Holds {
        type Break = ();
        fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
            match q.with.is_some() {
                true => ControlFlow::Break(()),
                false => ControlFlow::Continue(()),
            }
        }
        fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
            match t {
                TableFactor::Derived { .. } => ControlFlow::Break(()),
                _ => ControlFlow::Continue(()),
            }
        }
    }
    Visit::visit(q, &mut Holds).is_break()
}

struct Dropping<'c> {
    catalog: &'c Catalog,
}

impl VisitorMut for Dropping<'_> {
    type Break = ();

    /// A query's WITH queries and FROM subqueries, before the visit goes inside them: what the
    /// query reads of each read with that one taken out of it.
    fn pre_visit_query(&mut self, q: &mut Query) -> ControlFlow<()> {
        let ctes = q
            .with
            .as_ref()
            .filter(|w| !w.recursive)
            .map_or(0, |w| w.cte_tables.len());
        for i in 0..ctes {
            let Some(w) = q.with.as_mut() else {
                break;
            };
            let held = std::mem::replace(&mut w.cte_tables[i].query, hole());
            let reads = Reads::of(q);
            let Some(w) = q.with.as_mut() else {
                break;
            };
            let cte = &mut w.cte_tables[i];
            cte.query = held;
            let name = ident(&cte.alias.name);
            let refs = reads.factors.get(&name).cloned().unwrap_or_default();
            let in_place = match cte.materialized {
                Some(CteAsMaterialized::Materialized) => false,
                Some(CteAsMaterialized::NotMaterialized) => true,
                None => refs.len() == 1,
            };
            let release = self.catalog.release;
            let Some(natural) = reads.natural_names(self.catalog) else {
                continue;
            };
            // A WITH query made apart leaves only pg_settings' value columns unmade, which make
            // nothing 18.6 refuses.
            let apart = !in_place && settings_alone(&cte.query);
            if reads.star
                || !in_place && !apart
                || refs.is_empty()
                || volatile(&cte.query, release)
            {
                continue;
            }
            let renames: Vec<String> = cte.alias.columns.iter().map(|c| ident(&c.name)).collect();
            let read = Read {
                reads: &reads,
                natural: &natural,
                refs: &refs,
                renames: &renames,
            };
            left_unmade(&mut cte.query, &read, self.catalog, apart);
        }
        let mut i = 0;
        while let Some(TableFactor::Derived { subquery, .. }) = nth_derived(q, i) {
            let held = std::mem::replace(subquery, hole());
            let reads = Reads::of(q);
            let Some(TableFactor::Derived {
                subquery, alias, ..
            }) = nth_derived(q, i)
            else {
                break;
            };
            *subquery = held;
            i += 1;
            let Some(natural) = reads.natural_names(self.catalog) else {
                continue;
            };
            let Some(a) = alias.as_ref().filter(|_| !reads.star) else {
                continue;
            };
            let refs = vec![(
                ident(&a.name),
                a.columns.iter().map(|c| ident(&c.name)).collect(),
            )];
            let read = Read {
                reads: &reads,
                natural: &natural,
                refs: &refs,
                renames: &[],
            };
            left_unmade(subquery, &read, self.catalog, false);
        }
        ControlFlow::Continue(())
    }
}

/// What reads a subquery's outputs: the words of the query around it, the names a NATURAL join
/// there may meet, and the names the subquery goes by, with those its columns take.
struct Read<'r> {
    reads: &'r Reads,
    natural: &'r HashSet<String>,
    refs: &'r [(String, Vec<String>)],
    renames: &'r [String],
}

impl Read<'_> {
    /// Whether its `i`th output, named `own` by its query, is read under any name it goes by.
    fn read(&self, i: usize, own: &str) -> bool {
        self.refs.iter().any(|(alias, columns)| {
            let name = columns
                .get(i)
                .or_else(|| self.renames.get(i))
                .map_or(own, String::as_str);
            self.reads.bare.contains(name)
                || self.natural.contains(name)
                || self.reads.qualified.get(alias).is_some_and(|n| n.contains(name))
        })
    }
}

/// A query standing in a subquery's place while what is around it is read.
fn hole() -> Box<Query> {
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    match sqlparser::parser::Parser::parse_sql(&dialect, "SELECT NULL")
        .ok()
        .and_then(|mut s| s.pop())
    {
        Some(sqlparser::ast::Statement::Query(q)) => q,
        _ => unreachable!("a query"),
    }
}

/// The `i`th FROM subquery of a query's own SELECTs.
fn nth_derived(q: &mut Query, i: usize) -> Option<&mut TableFactor> {
    let mut selects = Vec::new();
    selects_of(q.body.as_mut(), &mut selects);
    selects
        .into_iter()
        .flat_map(|s| s.from.iter_mut())
        .flat_map(factors_of)
        .filter(|f| matches!(f, TableFactor::Derived { .. }))
        .nth(i)
}

/// The SELECTs a query's body is made of, its set operations' arms among them, but those of a
/// parenthesized query, a query of its own.
fn selects_of<'s>(body: &'s mut SetExpr, out: &mut Vec<&'s mut Select>) {
    match body {
        SetExpr::Select(s) => out.push(s),
        SetExpr::SetOperation { left, right, .. } => {
            selects_of(left, out);
            selects_of(right, out);
        }
        _ => {}
    }
}

/// The relations of one FROM item: its first, and each joined to it, inside parentheses too.
fn factors_of(t: &mut TableWithJoins) -> Vec<&mut TableFactor> {
    let mut out = Vec::new();
    fn factor<'f>(f: &'f mut TableFactor, out: &mut Vec<&'f mut TableFactor>) {
        match f {
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                factor(&mut table_with_joins.relation, out);
                for j in &mut table_with_joins.joins {
                    factor(&mut j.relation, out);
                }
            }
            f => out.push(f),
        }
    }
    factor(&mut t.relation, &mut out);
    for j in &mut t.joins {
        factor(&mut j.relation, &mut out);
    }
    out
}

/// Each output of a subquery nothing reads under any of the names it goes by, written to be left
/// unmade.
fn left_unmade(sub: &mut Query, read: &Read, catalog: &Catalog, values_only: bool) {
    if read.refs.iter().any(|(a, _)| read.reads.whole.contains(a)) {
        return;
    }
    let read = |i: usize, own: &str| read.read(i, own);
    let release = catalog.release;
    let sorted: Vec<Expr> = match sub.order_by.as_ref().map(|o| &o.kind) {
        Some(OrderByKind::Expressions(os)) => os.iter().map(|o| o.expr.clone()).collect(),
        _ => Vec::new(),
    };
    let whole = sorted.is_empty() && sub.limit_clause.is_none() && sub.fetch.is_none();
    let arms = whole && union_all(&sub.body) && arms_typed_alike(sub, catalog);
    match sub.body.as_mut() {
        SetExpr::Select(s) => {
            let settings = settings_starred(s);
            let names = own_names(s);
            unmade_items(s, &names, &sorted, &read, release, settings, values_only);
        }
        body @ SetExpr::SetOperation { .. } if arms && !values_only => {
            let mut selects = Vec::new();
            selects_of(body, &mut selects);
            // An arm's columns go by the first arm's names.
            let names = selects.first().map(|s| own_names(s)).unwrap_or_default();
            for s in selects {
                let settings = settings_starred(s);
                unmade_items(s, &names, &[], &read, release, settings, false);
            }
        }
        _ => {}
    }
}

/// Whether a SELECT reads pg_settings alone, whose value columns are each a setting's value read
/// through the refusal of a server's own (`sysviews::settings_values`): its star then written as
/// pg_settings' columns, as PostgreSQL's parser writes it out.
pub(super) fn settings_starred(s: &mut Select) -> bool {
    let alone = s.from.len() == 1 && s.from[0].joins.is_empty();
    let named = match &s.from.first().map(|t| &t.relation) {
        Some(TableFactor::Table {
            name, args: None, ..
        }) if alone => {
            let n = crate::catalog::object_name(name);
            super::sysviews::view_named(&n).as_deref() == Some("pg_catalog.pg_settings")
        }
        _ => false,
    };
    if !named {
        return false;
    }
    let star = |i: &SelectItem| matches!(i, SelectItem::Wildcard(o) if *o == Default::default());
    if s.projection.iter().any(star) {
        let mut out = Vec::new();
        for item in std::mem::take(&mut s.projection) {
            match star(&item) {
                true => out.extend(
                    super::settings::columns()
                        .iter()
                        .map(|(c, _)| SelectItem::UnnamedExpr(Expr::Identifier(Ident::new(*c)))),
                ),
                false => out.push(item),
            }
        }
        s.projection = out;
    }
    true
}

/// Whether a query is one SELECT reading pg_settings alone.
pub(super) fn settings_alone(q: &Query) -> bool {
    match q.body.as_ref() {
        SetExpr::Select(s) => settings_starred(&mut s.as_ref().clone()),
        _ => false,
    }
}

/// Whether each arm of a set operation, read alone, gives its columns the types the operation
/// gives them: only then are its arms read as subqueries of their own, each leaving its outputs.
/// Each arm's columns are typed over the file's tables its FROM names, a literal of no type taking
/// the operation's; an arm over anything else is not known so.
fn arms_typed_alike(sub: &Query, catalog: &Catalog) -> bool {
    use crate::catalog::ColumnType;
    let arm_types = |s: &Select| -> Option<Vec<Option<ColumnType>>> {
        let mut columns: Vec<(Option<String>, String, Option<ColumnType>)> = Vec::new();
        for t in &s.from {
            for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
                let TableFactor::Table {
                    name,
                    alias,
                    args: None,
                    ..
                } = f
                else {
                    return None;
                };
                let n = crate::catalog::object_name(name);
                let table = catalog.table(&n).or_else(|| {
                    catalog
                        .tables
                        .iter()
                        .find(|x| !n.contains('.') && x.name.rsplit('.').next() == Some(n.as_str()))
                })?;
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    return None;
                }
                let by = match alias {
                    Some(a) => ident(&a.name),
                    None => n.rsplit('.').next().unwrap_or(&n).to_string(),
                };
                columns.extend(
                    table
                        .own_columns()
                        .iter()
                        .map(|c| (Some(by.clone()), c.name.clone(), Some(c.ty))),
                );
            }
        }
        s.projection
            .iter()
            .map(|i| match i {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    let types = pipeline::typed_on(e, &columns, catalog.release).ok()?;
                    match types.get(&(e as *const Expr)) {
                        Some(t) => Some(Some(super::typed::unmodified(*t))),
                        // Only a literal is of no type.
                        None if pipeline::quoted(e)
                            || matches!(e, Expr::Value(v) if v.value == Value::Null) =>
                        {
                            Some(None)
                        }
                        None => None,
                    }
                }
                _ => None,
            })
            .collect()
    };
    let mut body = sub.body.as_ref().clone();
    let mut selects = Vec::new();
    selects_of(&mut body, &mut selects);
    let Some(arms) = selects
        .iter()
        .map(|s| arm_types(s))
        .collect::<Option<Vec<_>>>()
    else {
        return false;
    };
    let width = arms.first().map_or(0, Vec::len);
    if arms.iter().any(|a| a.len() != width) {
        return false;
    }
    (0..width).all(|c| {
        let column: Vec<Option<ColumnType>> = arms.iter().map(|a| a[c]).collect();
        match super::typed::common_type(&column, "UNION") {
            Ok(whole) => {
                let whole = super::typed::unmodified(whole);
                column.iter().all(|t| t.is_none_or(|t| t == whole))
            }
            Err(_) => false,
        }
    })
}

/// The name each item of a SELECT gives its column; none for a star.
fn own_names(s: &Select) -> Vec<Option<String>> {
    s.projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) => Some(pipeline::default_name(e)),
            SelectItem::ExprWithAlias { alias, .. } => Some(ident(alias)),
            _ => None,
        })
        .collect()
}

/// Whether a set operation is UNION ALLs alone, its arms SELECTs.
fn union_all(b: &SetExpr) -> bool {
    match b {
        SetExpr::Select(_) => true,
        SetExpr::SetOperation {
            op: SetOperator::Union,
            set_quantifier: SetQuantifier::All,
            left,
            right,
        } => union_all(left) && union_all(right),
        _ => false,
    }
}

/// Each item of one SELECT nothing reads, and that its query may leave unmade, so written.
fn unmade_items(
    s: &mut Select,
    names: &[Option<String>],
    sorted: &[Expr],
    read: &dyn Fn(usize, &str) -> bool,
    release: u8,
    settings: bool,
    values_only: bool,
) {
    // A value column of pg_settings is read through its refusal: computed, though written alone.
    let value = |e: &Expr| {
        settings && column_of(e).is_some_and(|c| super::settings::VALUES.contains(&c.as_str()))
    };
    if matches!(s.distinct, Some(Distinct::Distinct)) {
        return;
    }
    if names.len() != s.projection.len()
        || names.iter().any(Option::is_none)
        || s.projection.iter().any(|i| {
            matches!(
                i,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
            )
        })
    {
        return;
    }
    let mut keyed: Vec<&Expr> = sorted.iter().collect();
    if let GroupByExpr::Expressions(g, _) = &s.group_by {
        keyed.extend(g);
    }
    if let Some(Distinct::On(keys)) = &s.distinct {
        keyed.extend(keys);
    }
    // A column it holds as it is, of a subquery of its FROM that computes it, is left unmade too,
    // so that nothing reads that one's.
    let mut computed: HashSet<String> = HashSet::new();
    for t in &s.from {
        for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Derived {
                subquery, alias, ..
            } = f
            else {
                continue;
            };
            let SetExpr::Select(below) = subquery.body.as_ref() else {
                continue;
            };
            let columns: Vec<String> = alias
                .iter()
                .flat_map(|a| &a.columns)
                .map(|c| ident(&c.name))
                .collect();
            for (k, (item, own)) in below.projection.iter().zip(own_names(below)).enumerate() {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    if !read_as_written(e) {
                        computed.extend(columns.get(k).cloned().or(own));
                    }
                }
            }
        }
    }
    let mut unmade = Vec::new();
    for (i, item) in s.projection.iter().enumerate() {
        let e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => continue,
        };
        let own = names[i].clone().unwrap_or_default();
        let by_it = |k: &&Expr| {
            *k == e
                || matches!(k, Expr::Value(v) if matches!(&v.value, Value::Number(n, _) if n.parse::<usize>().ok() == Some(i + 1)))
                || matches!(k, Expr::Identifier(x) if ident(x) == own)
        };
        if unmade_already(e)
            || read_as_written(e)
                && !column_of(e).is_some_and(|c| computed.contains(&c))
                && !value(e)
            || values_only && !value(e)
            || read(i, &own)
            || keyed.iter().any(by_it)
            || pipeline::holds_set(e)
            || reads_unfiltered_arguments(e)
            || calls_volatile(e, release)
        {
            continue;
        }
        unmade.push((i, own));
    }
    for (i, own) in unmade {
        let item = std::mem::replace(
            &mut s.projection[i],
            SelectItem::UnnamedExpr(Expr::Value(Value::Null.into())),
        );
        let mut e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => unreachable!("an item of an expression"),
        };
        let _ = VisitMut::visit(&mut e, &mut NeverFolded { depth: 0 });
        s.projection[i] = SelectItem::ExprWithAlias {
            expr: unmade_of(e),
            alias: Ident::with_quote('"', own),
        };
    }
}

/// Whether an expression calls what reads an argument it computes that an aggregate's FILTER cannot
/// keep from being read: a window not an aggregate's, or an ordered-set aggregate's direct one.
fn reads_unfiltered_arguments(e: &Expr) -> bool {
    sqlparser::ast::visit_expressions(e, |x| match x {
        Expr::Function(f)
            if (f.over.is_some() && !pipeline::is_aggregate(&pipeline::function_name(f))
                || !f.within_group.is_empty())
                && pipeline::parts_of(x).into_iter().any(|a| !read_as_written(a)) =>
        {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

/// Each aggregate an expression calls as its query's own, written to fold no row.
struct NeverFolded {
    depth: usize,
}

impl VisitorMut for NeverFolded {
    type Break = ();

    fn pre_visit_query(&mut self, _q: &mut Query) -> ControlFlow<()> {
        self.depth += 1;
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _q: &mut Query) -> ControlFlow<()> {
        self.depth -= 1;
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        if let Expr::Function(f) = e {
            if self.depth == 0
                && (pipeline::is_aggregate(&pipeline::function_name(f))
                    || !f.within_group.is_empty())
            {
                f.filter = Some(Box::new(Expr::Value(Value::Boolean(false).into())));
            }
        }
        ControlFlow::Continue(())
    }
}

/// An expression written to be left unmade: `CASE WHEN false THEN e END`.
fn unmade_of(e: Expr) -> Expr {
    Expr::Case {
        case_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
        end_token: sqlparser::ast::helpers::attached_token::AttachedToken::empty(),
        operand: None,
        conditions: vec![CaseWhen {
            condition: Expr::Value(Value::Boolean(false).into()),
            result: e,
        }],
        else_result: None,
    }
}

/// The name of the column an expression is, parentheses passed over.
fn column_of(e: &Expr) -> Option<String> {
    match e {
        Expr::Nested(x) => column_of(x),
        Expr::Identifier(i) => Some(ident(i)),
        Expr::CompoundIdentifier(parts) => parts.last().map(ident),
        _ => None,
    }
}

/// Whether an expression is a column or a literal, which computes nothing: held as it is.
pub(super) fn read_as_written(e: &Expr) -> bool {
    match e {
        Expr::Nested(x) => read_as_written(x),
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Value(_) => true,
        _ => false,
    }
}

/// Whether an expression is one written to be left unmade, which nothing reads inside.
fn unmade_already(e: &Expr) -> bool {
    matches!(e, Expr::Case { operand: None, conditions, else_result: None, .. }
        if conditions.len() == 1 && pipeline::written_truth(&conditions[0].condition) == Some(false))
}

/// Whether an expression calls what may be volatile.
pub(super) fn calls_volatile(e: &Expr, release: u8) -> bool {
    sqlparser::ast::visit_expressions(e, |x| match x {
        Expr::Function(f)
            if super::own::may_be_volatile(&pipeline::function_name(f), release)
                || pipeline::draws(x) =>
        {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

/// Whether a query calls what may be volatile anywhere.
fn volatile(q: &Query, release: u8) -> bool {
    sqlparser::ast::visit_expressions(q, |x| match x {
        Expr::Function(f)
            if super::own::may_be_volatile(&pipeline::function_name(f), release)
                || pipeline::draws(x) =>
        {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

/// What a query can read of the relations its FROMs name, as its words say it: each name written
/// alone; each written after a relation's name; each relation read whole or by a star; whether a
/// star of no relation or a NATURAL join stands in it; and each relation a FROM names, with the
/// alias it goes by there and the names that alias gives its columns. Nothing inside an expression
/// left unmade is read. For a NATURAL join, the columns each relation of its FROMs may give: each
/// table's by its name, each subquery's and each WITH query's by their own names, none known of
/// any other.
#[derive(Default)]
struct Reads {
    bare: HashSet<String>,
    qualified: HashMap<String, HashSet<String>>,
    whole: HashSet<String>,
    star: bool,
    natural: bool,
    factors: HashMap<String, Vec<(String, Vec<String>)>>,
    skipping: Option<*const Expr>,
    tables: Vec<String>,
    derived: Vec<Option<Vec<String>>>,
    ctes: HashMap<String, Option<Vec<String>>>,
    opaque: bool,
}

impl Reads {
    fn of(q: &Query) -> Reads {
        let mut r = Reads::default();
        let _ = Visit::visit(q, &mut r);
        r
    }

    fn name(&mut self, n: &ObjectName) {
        if let Some(last) = n.0.last().and_then(|p| p.as_ident()) {
            self.whole.insert(ident(last));
        }
    }

    /// The names a NATURAL join among its FROMs may meet: every column of every relation they
    /// name, none where it holds no NATURAL join; nothing known where a relation's are not.
    fn natural_names(&self, catalog: &Catalog) -> Option<HashSet<String>> {
        let mut out = HashSet::new();
        if !self.natural {
            return Some(out);
        }
        if self.opaque {
            return None;
        }
        for t in &self.tables {
            if let Some(c) = self.ctes.get(t).filter(|_| !t.contains('.')) {
                out.extend(c.clone()?);
            } else if let Some(table) = catalog.table(t).or_else(|| {
                catalog
                    .tables
                    .iter()
                    .find(|x| !t.contains('.') && x.name.rsplit('.').next() == Some(t.as_str()))
            }) {
                out.extend(table.own_columns().iter().map(|c| c.name.clone()));
            } else if let Some(v) = catalog.view(t) {
                out.extend(v.columns.iter().cloned());
            } else {
                return None;
            }
        }
        for d in &self.derived {
            out.extend(d.clone()?);
        }
        Some(out)
    }
}

/// The names a query's columns go by: its first SELECT's, renamed first by `renames`; none known
/// where it returns a star.
pub(super) fn names_of(q: &Query, renames: &[String]) -> Option<Vec<String>> {
    let mut body = q.body.as_ref();
    while let SetExpr::SetOperation { left, .. } = body {
        body = left;
    }
    let own = match body {
        SetExpr::Select(s) => own_names(s).into_iter().collect::<Option<Vec<_>>>()?,
        SetExpr::Query(q) => names_of(q, &[])?,
        _ => return None,
    };
    Some(
        own.into_iter()
            .enumerate()
            .map(|(i, n)| renames.get(i).cloned().unwrap_or(n))
            .collect(),
    )
}

impl Visitor for Reads {
    type Break = ();

    fn pre_visit_query(&mut self, q: &Query) -> ControlFlow<()> {
        if self.skipping.is_some() {
            return ControlFlow::Continue(());
        }
        fn selects<'s>(b: &'s SetExpr, out: &mut Vec<&'s Select>) {
            match b {
                SetExpr::Select(s) => out.push(s),
                SetExpr::SetOperation { left, right, .. } => {
                    selects(left, out);
                    selects(right, out);
                }
                _ => {}
            }
        }
        for cte in q.with.iter().flat_map(|w| &w.cte_tables) {
            let renames: Vec<String> = cte.alias.columns.iter().map(|c| ident(&c.name)).collect();
            self.ctes
                .insert(ident(&cte.alias.name), names_of(&cte.query, &renames));
        }
        let mut found = Vec::new();
        selects(&q.body, &mut found);
        for s in found {
            for item in &s.projection {
                match item {
                    SelectItem::Wildcard(_) => self.star = true,
                    SelectItem::QualifiedWildcard(
                        SelectItemQualifiedWildcardKind::ObjectName(n),
                        _,
                    ) => self.name(n),
                    SelectItem::QualifiedWildcard(..) => self.star = true,
                    _ => {}
                }
            }
            for j in s.from.iter().flat_map(|t| &t.joins) {
                let constraint = match &j.join_operator {
                    JoinOperator::Join(c)
                    | JoinOperator::Inner(c)
                    | JoinOperator::Left(c)
                    | JoinOperator::LeftOuter(c)
                    | JoinOperator::Right(c)
                    | JoinOperator::RightOuter(c)
                    | JoinOperator::FullOuter(c) => Some(c),
                    _ => None,
                };
                match constraint {
                    Some(JoinConstraint::Natural) => self.natural = true,
                    Some(JoinConstraint::Using(names)) => {
                        for n in names {
                            if let Some(last) = n.0.last().and_then(|p| p.as_ident()) {
                                self.bare.insert(ident(last));
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_table_factor(&mut self, t: &TableFactor) -> ControlFlow<()> {
        if self.skipping.is_some() {
            return ControlFlow::Continue(());
        }
        match t {
            TableFactor::Table { name, args, .. } if args.is_none() => {
                self.tables.push(crate::catalog::object_name(name));
            }
            TableFactor::Derived {
                subquery, alias, ..
            } => {
                let renames: Vec<String> = alias
                    .iter()
                    .flat_map(|a| &a.columns)
                    .map(|c| ident(&c.name))
                    .collect();
                self.derived.push(names_of(subquery, &renames));
            }
            TableFactor::NestedJoin { .. } => {}
            _ => self.opaque = true,
        }
        if let TableFactor::Table { name, alias, .. } = t {
            if let Some(last) = name.0.last().and_then(|p| p.as_ident()) {
                let named = ident(last);
                let (by, columns) = match alias {
                    Some(a) => (
                        ident(&a.name),
                        a.columns.iter().map(|c| ident(&c.name)).collect(),
                    ),
                    None => (named.clone(), Vec::new()),
                };
                self.factors.entry(named).or_default().push((by, columns));
            }
        }
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if self.skipping.is_some() {
            return ControlFlow::Continue(());
        }
        if unmade_already(e) {
            self.skipping = Some(e as *const Expr);
            return ControlFlow::Continue(());
        }
        match e {
            Expr::Identifier(i) => {
                self.bare.insert(ident(i));
                self.whole.insert(ident(i));
            }
            Expr::CompoundIdentifier(parts) => {
                for p in parts.iter().skip(1) {
                    self.bare.insert(ident(p));
                }
                for w in parts.windows(2) {
                    self.qualified
                        .entry(ident(&w[0]))
                        .or_default()
                        .insert(ident(&w[1]));
                }
            }
            Expr::Wildcard(_) => self.star = true,
            Expr::QualifiedWildcard(n, _) => self.name(n),
            Expr::Function(f) => {
                if let FunctionArguments::List(l) = &f.args {
                    for a in &l.args {
                        let (sqlparser::ast::FunctionArg::Unnamed(x)
                        | sqlparser::ast::FunctionArg::Named { arg: x, .. }
                        | sqlparser::ast::FunctionArg::ExprNamed { arg: x, .. }) = a;
                        if let FunctionArgExpr::QualifiedWildcard(n) = x {
                            self.name(n);
                        }
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if self.skipping == Some(e as *const Expr) {
            self.skipping = None;
        }
        ControlFlow::Continue(())
    }
}
