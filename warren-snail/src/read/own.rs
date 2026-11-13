//! The functions a snail's own DDL declares, called wherever a statement calls them, as PostgreSQL
//! 18.6 calls a function written in SQL. Its planner inlines one where it can, and then the body
//! reads each argument where it reaches it, a CASE's branch not taken reading none. Otherwise it
//! calls it: each argument computed once, left to right, and a STRICT function answering NULL for a
//! NULL argument without its body. Which of the two, it decides from the body and the arguments, by
//! the strictness, volatility and cost its catalog gives each function, operator and cast they
//! hold; those are read here from the catalog the snail carries for the file's release. A body
//! that reads relations is never inlined: its statement is run through the snail the caller reads,
//! each argument a constant in it, and its first row is the call's value.

use super::pipeline::{Casings, Computed, Frame, Value};
use crate::catalog::{ColumnType, Function};
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr, UnaryOperator};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ops::ControlFlow;
use std::rc::Rc;
use std::sync::{Arc, Mutex, OnceLock};

/// A function of the file's, made ready to be called.
pub(crate) struct Own {
    /// Its name as the catalog holds it: `s.plus`, or `plus` for one of public.
    name: String,
    args: Vec<ColumnType>,
    /// Each parameter's name; none where it has none.
    names: Vec<Option<String>>,
    /// The DEFAULTs of its last parameters, each typed on its own.
    defaults: Vec<Defaulted>,
    returns: ColumnType,
    strict: bool,
    volatility: char,
    /// Its body, each parameter named as the column `"$1"`, `"$2"` and on.
    body: Box<Expr>,
    /// Its body's expressions' types, its parameters of the types it declares.
    types: super::typed::Types,
    /// How often its body names each parameter.
    uses: Vec<usize>,
    /// What its body holds, by the catalog's word on each function, operator and cast in it.
    held: Props,
    /// Where its body reads relations (a subquery, or a statement more than one expression), the
    /// statement a call asks the snail, as written.
    question: Option<String>,
    /// Why it is not called, where its body is not read.
    refused: Option<String>,
}

/// A parameter's DEFAULT: its expression and the types its parts read, read over no row.
struct Defaulted {
    expr: Box<Expr>,
    types: super::typed::Types,
}

/// Where a parameter's value comes from in one call: the call's argument at its place, or the
/// parameter's DEFAULT at its place among them.
#[derive(Clone, Copy, Debug)]
enum Given {
    Arg(usize),
    Default(usize),
}

/// What an expression holds, as 18.6's planner reads it before it inlines a function.
#[derive(Clone, Copy, Debug, Default)]
struct Props {
    /// The most volatile function in it: 0 immutable, 1 stable, 2 volatile.
    volatility: u8,
    /// Whether anything in it may answer other than NULL for a NULL input.
    nonstrict: bool,
    /// Its functions' costs, as pg_proc's procost counts them.
    cost: f64,
    /// Whether it holds a subquery.
    sublink: bool,
    /// Whether it holds an aggregate, a window function or a set-returning function.
    folds: bool,
}

impl Props {
    fn with(&mut self, p: Props) {
        self.volatility = self.volatility.max(p.volatility);
        self.nonstrict |= p.nonstrict;
        self.cost += p.cost;
        self.sublink |= p.sublink;
        self.folds |= p.folds;
    }
}

/// The functions held, in the DDL's order.
type Held = Rc<Vec<Rc<Own>>>;

thread_local! {
    /// The functions of the snail whose statement is under way on this thread.
    static OWN: RefCell<Held> = RefCell::new(Rc::new(Vec::new()));
    /// The functions last made, by what made them.
    static MADE: RefCell<Option<(String, Held)>> = const { RefCell::new(None) };
}

/// A snail's functions held while a statement runs; those before come back where it is dropped.
pub(crate) struct Holding(Rc<Vec<Rc<Own>>>);

impl Drop for Holding {
    fn drop(&mut self) {
        let before = std::mem::take(&mut self.0);
        OWN.with(|o| *o.borrow_mut() = before);
    }
}

/// Holds a catalog's functions for the statement under way, each body typed once.
pub(crate) fn holding(functions: &[Function], release: u8) -> Holding {
    let before = OWN.with(|o| o.replace(Rc::new(Vec::new())));
    let held = Holding(before);
    if functions.is_empty() {
        return held;
    }
    let key = functions
        .iter()
        .map(|f| {
            format!(
                "{}\u{1}{:?}\u{1}{:?}\u{1}{}{}\u{1}{}\u{1}{:?}\u{1}{}\u{1}{:?}",
                f.name,
                f.args,
                f.returns,
                f.strict,
                f.volatility,
                f.statement,
                f.arg_names,
                f.defaults.iter().map(|d| d.to_string()).collect::<Vec<_>>().join("\u{3}"),
                f.form
            )
        })
        .collect::<Vec<_>>()
        .join("\u{2}")
        + &format!("\u{2}{release}");
    if let Some(all) = MADE.with(|m| {
        m.borrow()
            .as_ref()
            .filter(|(k, _)| *k == key)
            .map(|(_, all)| all.clone())
    }) {
        OWN.with(|o| *o.borrow_mut() = all);
        return held;
    }
    let all = made_all(functions, release, None);
    OWN.with(|o| *o.borrow_mut() = all.clone());
    MADE.with(|m| *m.borrow_mut() = Some((key, all)));
    held
}

/// Every function made ready, in the DDL's order. Each is held by its signature first, so a body
/// may call any of them, itself among them; each body is then typed with those before it made.
fn made_all(functions: &[Function], release: u8, relations: Option<&Relations>) -> Rc<Vec<Rc<Own>>> {
    let stubs: Vec<Rc<Own>> = functions.iter().map(|f| Rc::new(stub(f))).collect();
    OWN.with(|o| *o.borrow_mut() = Rc::new(stubs));
    for (i, f) in functions.iter().enumerate() {
        let own = Rc::new(made(f, release, relations));
        OWN.with(|o| {
            let mut all = (**o.borrow()).clone();
            all[i] = own;
            *o.borrow_mut() = Rc::new(all);
        });
    }
    OWN.with(|o| o.borrow().clone())
}

/// A function held by its signature alone, before its body is typed.
fn stub(f: &Function) -> Own {
    Own {
        name: f.name.clone(),
        args: f.args.clone(),
        names: f.arg_names.clone(),
        defaults: f
            .defaults
            .iter()
            .map(|e| Defaulted {
                expr: Box::new(e.clone()),
                types: Default::default(),
            })
            .collect(),
        returns: f.returns,
        strict: f.strict,
        volatility: f.volatility,
        body: Box::new(f.body.clone()),
        types: Default::default(),
        uses: vec![1; f.args.len()],
        held: Props::default(),
        question: None,
        refused: Some("its body is not made yet".into()),
    }
}

/// A relation a body's subquery may read: its columns' names and types, a view's none, and whether
/// it is a table, whose system columns it reads too.
pub(crate) struct Readable {
    pub columns: Vec<String>,
    pub types: Vec<Option<ColumnType>>,
    pub table: bool,
}

/// The relations a body's subqueries may read, each by its full name, as the catalog the function
/// is made over holds them.
pub(crate) type Relations = HashMap<String, Readable>;

/// A catalog's relations as a body's subqueries read them: each table and each view.
pub(crate) fn relations_of(catalog: &crate::catalog::Catalog) -> Relations {
    let tables = catalog.tables.iter().map(|t| {
        let columns = t.columns.iter().map(|c| c.name.clone()).collect();
        let types = t.columns.iter().map(|c| Some(c.ty)).collect();
        (crate::oids::full(&t.name), Readable { columns, types, table: true })
    });
    let views = catalog.views.iter().map(|v| {
        let columns = v.columns.clone();
        let types = vec![None; v.columns.len()];
        (crate::oids::full(&v.name), Readable { columns, types, table: false })
    });
    tables.chain(views).collect()
}

/// How many columns a question returns, as its words and `relations` tell: a SELECT's items, a
/// star's the relations it names; a set operation's first arm's; a VALUES list's first row's. None
/// where a star names what is not known here.
pub(crate) fn columns_written(q: &sqlparser::ast::Query, relations: &Relations) -> Option<usize> {
    use sqlparser::ast::{SelectItem, SelectItemQualifiedWildcardKind, SetExpr};
    fn of(body: &SetExpr, with: &[String], relations: &Relations) -> Option<usize> {
        match body {
            SetExpr::Select(s) => {
                let scopes = Scopes {
                    levels: Vec::new(),
                    with: vec![with.to_vec()],
                };
                let items = || match scopes.items(s, relations) {
                    Level::Items(i) => Some(i),
                    Level::Nothing => Some(Vec::new()),
                    Level::Unknown => None,
                };
                let mut n: usize = 0;
                for item in &s.projection {
                    n += match item {
                        SelectItem::Wildcard(_) => {
                            items()?.iter().map(|(_, c, _)| c.len()).sum::<usize>()
                        }
                        SelectItem::QualifiedWildcard(
                            SelectItemQualifiedWildcardKind::ObjectName(name),
                            _,
                        ) => {
                            let last = name.0.last()?.as_ident().map(crate::catalog::ident)?;
                            let found = items()?.into_iter().find(|(n, _, _)| *n == last)?;
                            found.1.len()
                        }
                        SelectItem::QualifiedWildcard(..) => return None,
                        _ => 1,
                    };
                }
                Some(n)
            }
            SetExpr::SetOperation { left, .. } => of(left, with, relations),
            SetExpr::Values(v) => v.rows.first().map(|r| r.content.len()),
            SetExpr::Query(q) => columns_written(q, relations),
            _ => None,
        }
    }
    let with: Vec<String> = q
        .with
        .iter()
        .flat_map(|w| &w.cte_tables)
        .map(|c| crate::catalog::ident(&c.alias.name))
        .collect();
    of(&q.body, &with, relations)
}

/// What a query's FROM gives the names inside it.
enum Level {
    /// It reads no relation.
    Nothing,
    /// Each FROM item's name, its columns' names, and whether it is a table.
    Items(Vec<(String, Vec<String>, bool)>),
    /// A FROM item whose columns are not known here: a subquery, a function, a WITH query, or a
    /// relation the catalog given holds none of, or no catalog given.
    Unknown,
}

/// The query levels a walk of a body is inside, and the WITH queries each names.
#[derive(Default)]
struct Scopes {
    levels: Vec<Level>,
    with: Vec<Vec<String>>,
}

impl Scopes {
    fn enter(&mut self, q: &sqlparser::ast::Query, relations: Option<&Relations>) {
        use crate::catalog::ident;
        let named = q.with.iter().flat_map(|w| &w.cte_tables);
        self.with.push(named.map(|c| ident(&c.alias.name)).collect());
        let level = match (q.body.as_ref(), relations) {
            (sqlparser::ast::SetExpr::Select(s), Some(r)) if !s.from.is_empty() => self.items(s, r),
            (sqlparser::ast::SetExpr::Select(s), None) if !s.from.is_empty() => Level::Unknown,
            _ => Level::Nothing,
        };
        self.levels.push(level);
    }

    fn leave(&mut self) {
        self.levels.pop();
        self.with.pop();
    }

    /// The FROM items of a SELECT, each named as its alias or its relation's name names it.
    fn items(&self, s: &sqlparser::ast::Select, relations: &Relations) -> Level {
        use crate::catalog::ident;
        use sqlparser::ast::TableFactor;
        let mut out = Vec::new();
        for t in &s.from {
            for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
                let TableFactor::Table {
                    name,
                    alias,
                    args: None,
                    ..
                } = f
                else {
                    return Level::Unknown;
                };
                if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                    return Level::Unknown;
                }
                let parts: Vec<String> =
                    name.0.iter().filter_map(|p| p.as_ident().map(ident)).collect();
                let key = match parts.as_slice() {
                    [n] if self.with.iter().flatten().any(|w| w == n) => return Level::Unknown,
                    [n] => format!("public.{n}"),
                    [s, n] => format!("{s}.{n}"),
                    _ => return Level::Unknown,
                };
                let Some(r) = relations.get(&key) else {
                    return Level::Unknown;
                };
                let refname = match alias {
                    Some(a) => ident(&a.name),
                    None => parts.last().cloned().unwrap_or_default(),
                };
                out.push((refname, r.columns.clone(), r.table));
            }
        }
        Level::Items(out)
    }

    /// Whether the walk is inside a query that reads a relation.
    fn reading(&self) -> bool {
        self.levels.iter().any(|l| !matches!(l, Level::Nothing))
    }

    /// Whether every FROM item in reach is known.
    fn known(&self) -> bool {
        !self.levels.iter().any(|l| matches!(l, Level::Unknown))
    }

    /// The FROM items in reach, the innermost query's first.
    fn in_reach(&self) -> impl Iterator<Item = &(String, Vec<String>, bool)> {
        self.levels.iter().rev().flat_map(|l| match l {
            Level::Items(i) => i.as_slice(),
            _ => &[],
        })
    }

    /// Whether a FROM item in reach holds a column of that name.
    fn holds(&self, column: &str) -> bool {
        self.in_reach().any(|(_, columns, table)| {
            columns.iter().any(|c| c == column)
                || *table && crate::oids::SYSTEM_COLUMNS.contains(&column)
        })
    }

    /// The FROM item in reach of that name, the innermost first.
    fn item(&self, name: &str) -> Option<&(String, Vec<String>, bool)> {
        self.in_reach().find(|(n, _, _)| n == name)
    }
}

/// The names a body's walk refuses, in PostgreSQL's words: a name no parameter has, or no column of
/// a relation its subqueries read, a relation the body holds none of, a function neither the DDL nor
/// pg_catalog holds.
struct Unknowns<'a> {
    types: &'a super::typed::Types,
    relations: Option<&'a Relations>,
    carried: &'a Carried,
    scopes: Scopes,
    names_of_calls: std::collections::HashSet<*const Expr>,
    why: Option<String>,
}

impl Unknowns<'_> {
    /// A column a subquery names that no relation in reach holds, as PostgreSQL's
    /// `errorMissingColumn` refuses it; none where it is held, or where a relation in reach is not
    /// known here.
    fn missing(&self, e: &Expr) -> Option<String> {
        use crate::catalog::ident;
        if !self.scopes.known() {
            return None;
        }
        let reach: Vec<(String, Vec<String>)> = self
            .scopes
            .in_reach()
            .map(|(n, c, _)| (n.clone(), c.clone()))
            .collect();
        match e {
            Expr::Identifier(i) => {
                let n = ident(i);
                (!self.scopes.holds(&n)).then(|| {
                    let hint = super::hint::column_hint(&reach, None, &n).unwrap_or_default();
                    format!("column \"{n}\" does not exist{hint}")
                })
            }
            Expr::CompoundIdentifier(parts) if parts.len() == 2 => {
                let (q, n) = (ident(&parts[0]), ident(&parts[1]));
                match self.scopes.item(&q) {
                    None => Some(format!("missing FROM-clause entry for table \"{q}\"")),
                    Some((_, columns, table)) => {
                        let held = columns.iter().any(|c| *c == n)
                            || *table && crate::oids::SYSTEM_COLUMNS.contains(&n.as_str());
                        (!held).then(|| {
                            let hint =
                                super::hint::column_hint(&reach, Some(&q), &n).unwrap_or_default();
                            format!("column {q}.{n} does not exist{hint}")
                        })
                    }
                }
            }
            _ => None,
        }
    }
}

impl sqlparser::ast::Visitor for Unknowns<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &sqlparser::ast::Query) -> ControlFlow<()> {
        self.scopes.enter(q, self.relations);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &sqlparser::ast::Query) -> ControlFlow<()> {
        self.scopes.leave();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
        if self.names_of_calls.contains(&(e as *const Expr)) {
            return ControlFlow::Continue(());
        }
        self.names_of_calls.extend(argument_names(e));
        let parameter = |i: &sqlparser::ast::Ident| i.quote_style == Some('"') && i.value.starts_with('$');
        let found = match e {
            Expr::Identifier(i) if parameter(i) => None,
            Expr::Identifier(_) | Expr::CompoundIdentifier(_) if self.scopes.reading() => {
                self.missing(e)
            }
            Expr::Identifier(i) => {
                Some(format!("column \"{}\" does not exist", crate::catalog::ident(i)))
            }
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => Some(format!(
                "missing FROM-clause entry for table \"{}\"",
                crate::catalog::ident(&parts[parts.len() - 2])
            )),
            Expr::Function(f) => unknown_function(f, self.types, self.carried),
            _ => None,
        };
        match found {
            Some(w) => {
                self.why = Some(w);
                ControlFlow::Break(())
            }
            None => ControlFlow::Continue(()),
        }
    }
}

/// Whether a body names a column or a function the function cannot read, refused in PostgreSQL's
/// words, as `Unknowns` finds them; the names of its subqueries checked against `relations` where
/// given, as the function is made, and left to the call where not.
fn unknown_name(
    body: &Expr,
    types: &super::typed::Types,
    release: u8,
    relations: Option<&Relations>,
) -> Option<String> {
    let c = carried(release);
    let mut walk = Unknowns {
        types,
        relations,
        carried: &c,
        scopes: Scopes::default(),
        names_of_calls: Default::default(),
        why: None,
    };
    let _ = sqlparser::ast::Visit::visit(body, &mut walk);
    walk.why
}

/// A call of a function neither the DDL nor pg_catalog holds, refused as PostgreSQL refuses it.
fn unknown_function(
    f: &sqlparser::ast::Function,
    types: &super::typed::Types,
    c: &Carried,
) -> Option<String> {
    let called = crate::catalog::object_name(&f.name);
    let bare = called.strip_prefix("pg_catalog.").unwrap_or(&called);
    let known = found(&called).is_some()
        || !bare.contains('.') && (c.by_name.contains_key(bare) || SYNTAX.contains(&bare));
    (!known).then(|| {
        let args: Vec<Option<ColumnType>> = match &f.args {
            sqlparser::ast::FunctionArguments::List(l) => l
                .args
                .iter()
                .map(|a| match a {
                    sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(
                        x,
                    )) => types.get(&(x as *const Expr)).copied(),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        };
        super::typed::no_form(&called, &args).to_string()
    })
}

/// The calls PostgreSQL's grammar reads as syntax of its own, which pg_proc holds under no name.
const SYNTAX: &[&str] = &[
    "coalesce",
    "nullif",
    "greatest",
    "least",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    "current_user",
    "current_role",
    "session_user",
    "user",
    "current_catalog",
    "current_schema",
];

/// The parameters a body names, as PostgreSQL's parser finds them in a SQL function's body.
struct Naming<'a> {
    function: &'a str,
    names: &'a [Option<String>],
    to: &'a dyn Fn(usize) -> Expr,
    uses: Vec<usize>,
    beyond: Option<String>,
    /// The relations the body's subqueries may read, where the catalog is given.
    relations: Option<&'a Relations>,
    /// The queries the walk is inside, and what each one's FROM gives.
    scopes: Scopes,
    /// The names of named arguments met so far, which name a call's parameter, never a value.
    names_of_calls: std::collections::HashSet<*const Expr>,
}

/// The names a call gives its named arguments, which an expression's walk meets as expressions.
fn argument_names(e: &Expr) -> Vec<*const Expr> {
    match e {
        Expr::Function(f) => match &f.args {
            sqlparser::ast::FunctionArguments::List(l) => l
                .args
                .iter()
                .filter_map(|a| match a {
                    sqlparser::ast::FunctionArg::ExprNamed { name, .. } => Some(name as *const Expr),
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        },
        _ => Vec::new(),
    }
}

impl sqlparser::ast::VisitorMut for Naming<'_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut sqlparser::ast::Query) -> ControlFlow<()> {
        self.scopes.enter(q, self.relations);
        ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &mut sqlparser::ast::Query) -> ControlFlow<()> {
        self.scopes.leave();
        ControlFlow::Continue(())
    }

    fn pre_visit_expr(&mut self, e: &mut Expr) -> ControlFlow<()> {
        if self.names_of_calls.contains(&(e as *const Expr)) {
            return ControlFlow::Continue(());
        }
        self.names_of_calls.extend(argument_names(e));
        let named = |n: &str| self.names.iter().position(|x| x.as_deref() == Some(n));
        let at = match e {
            Expr::Value(v) => match &v.value {
                sqlparser::ast::Value::Placeholder(p) => {
                    match p.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
                        Some(n) if n >= 1 && n <= self.uses.len() => Some(n - 1),
                        _ => {
                            self.beyond.get_or_insert(format!("there is no parameter {p}"));
                            None
                        }
                    }
                }
                _ => None,
            },
            // Inside a subquery reading a relation, a column of a relation in reach comes before
            // a parameter of its name; where one in reach is not known here, the name is left.
            _ if self.scopes.reading() && !self.scopes.known() => None,
            Expr::Identifier(i) if self.scopes.holds(&crate::catalog::ident(i)) => None,
            Expr::Identifier(i) => named(&crate::catalog::ident(i)),
            Expr::CompoundIdentifier(parts)
                if parts.len() == 2
                    && crate::catalog::ident(&parts[0]) == self.function
                    && self.scopes.item(self.function).is_none() =>
            {
                named(&crate::catalog::ident(&parts[1]))
            }
            _ => None,
        };
        if let Some(i) = at {
            self.uses[i] += 1;
            *e = (self.to)(i + 1);
        }
        ControlFlow::Continue(())
    }
}

/// Each parameter a function's body names, by `$n`, by its name, or by the function's name and its
/// name, written as `to` writes the n-th; a name inside a subquery that reads a relation left to
/// the column of that name of a relation in reach, `relations` saying which hold it, and left
/// whole where no catalog is given. How often each is named, and the first `$n` past the last
/// parameter.
pub(crate) fn parameters_named<V: sqlparser::ast::VisitMut>(
    body: &mut V,
    function: &str,
    names: &[Option<String>],
    relations: Option<&Relations>,
    to: &dyn Fn(usize) -> Expr,
) -> (Vec<usize>, Option<String>) {
    let mut n = Naming {
        function: function.rsplit('.').next().unwrap_or(function),
        names,
        to,
        uses: vec![0; names.len()],
        beyond: None,
        relations,
        scopes: Scopes::default(),
        names_of_calls: Default::default(),
    };
    let _ = body.visit(&mut n);
    (n.uses, n.beyond)
}

/// A function made ready: its parameters named as columns, its body typed with them, the names of
/// its subqueries read over `relations` where given.
fn made(f: &Function, release: u8, relations: Option<&Relations>) -> Own {
    let mut body = Box::new(f.body.clone());
    let (uses, beyond) =
        parameters_named(body.as_mut(), &f.name, &f.arg_names, relations, &|n| {
            Expr::Identifier(sqlparser::ast::Ident::with_quote('"', format!("${n}")))
        });
    let columns: Vec<(Option<String>, String, Option<ColumnType>)> = f
        .args
        .iter()
        .enumerate()
        .map(|(i, t)| (None, format!("${}", i + 1), Some(*t)))
        .collect();
    let (types, refused) = match beyond {
        Some(why) => (Default::default(), Some(why)),
        None => match super::pipeline::typed_on(&body, &columns, release) {
            Ok(t) => {
                let why = unknown_name(&body, &t, release, relations);
                (t, why)
            }
            Err(e) => (Default::default(), Some(e.to_string())),
        },
    };
    let held = props(&body, &|x| types.get(&(x as *const Expr)).copied(), release);
    // A body that reads relations is called as its statement, which the snail asking it reads over
    // its own relations.
    let question = held.sublink.then(|| question_text(f));
    // Each DEFAULT typed over no row, as a call reads it in its argument's place.
    let mut refused = refused;
    let defaults = f
        .defaults
        .iter()
        .map(|e| {
            let expr = Box::new(e.clone());
            let types = match super::pipeline::typed_on(&expr, &[], release) {
                Ok(t) => t,
                Err(err) => {
                    refused.get_or_insert(err.to_string());
                    Default::default()
                }
            };
            Defaulted { expr, types }
        })
        .collect();
    Own {
        name: f.name.clone(),
        args: f.args.clone(),
        names: f.arg_names.clone(),
        defaults,
        returns: f.returns,
        strict: f.strict,
        volatility: f.volatility,
        body,
        types,
        uses,
        held,
        question,
        refused,
    }
}

/// A function's body as the statement a call runs: RETURN's expression, or a RETURN inside BEGIN
/// ATOMIC, selected; any other statement as written.
fn question_text(f: &Function) -> String {
    match f.form {
        crate::catalog::BodyForm::Return => format!("SELECT {}", f.statement),
        _ => match crate::ddl::returned(&f.statement) {
            Some(e) => format!("SELECT {e}"),
            None => f.statement.clone(),
        },
    }
}

/// The type a function's body gives, typed as 18.6 types it when the function is made, after
/// `before` and itself, its subqueries' names read over `relations`: refused where its parser
/// refuses the body. A quoted literal alone is text; a statement, or a subquery alone, its one
/// column's type.
pub(crate) fn body_type(
    before: &[Function],
    f: &Function,
    release: u8,
    relations: &Relations,
) -> Result<Option<ColumnType>> {
    let all: Vec<Function> = before.iter().cloned().chain([f.clone()]).collect();
    let _held = holding(&[], release);
    let made = made_all(&all, release, Some(relations));
    let own = made.last().expect("the function made");
    if let Some(why) = &own.refused {
        return Err(Error::new(why.clone()));
    }
    let mut body = own.body.as_ref();
    while let Expr::Nested(x) = body {
        body = x;
    }
    Ok(own
        .types
        .get(&(own.body.as_ref() as *const Expr))
        .copied()
        .or_else(|| match body {
            Expr::Subquery(q) => subquery_type(q, relations, &f.args, release),
            _ => None,
        })
        .or_else(|| super::pipeline::quoted(body).then_some(ColumnType::Text(None))))
}

/// The type of a subquery's one column, typed over the tables its FROM names and the parameters,
/// each named `"$1"` on: none where it reads what is not known here, or its column is not typed.
fn subquery_type(
    q: &sqlparser::ast::Query,
    relations: &Relations,
    args: &[ColumnType],
    release: u8,
) -> Option<ColumnType> {
    use crate::catalog::ident;
    use sqlparser::ast::{SelectItem, SetExpr, TableFactor};
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    let [SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. }] =
        s.projection.as_slice()
    else {
        return None;
    };
    if q.with.is_some() {
        return None;
    }
    let mut columns: Vec<super::typed::TypedColumn> = Vec::new();
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
            if alias.as_ref().is_some_and(|a| !a.columns.is_empty()) {
                return None;
            }
            let parts: Vec<String> = name.0.iter().filter_map(|p| p.as_ident().map(ident)).collect();
            let key = match parts.as_slice() {
                [n] => format!("public.{n}"),
                [s, n] => format!("{s}.{n}"),
                _ => return None,
            };
            let r = relations.get(&key)?;
            let by = match alias {
                Some(a) => ident(&a.name),
                None => parts.last().cloned().unwrap_or_default(),
            };
            for (c, t) in r.columns.iter().zip(&r.types) {
                columns.push((Some(by.clone()), c.clone(), *t));
            }
        }
    }
    columns.extend(
        args.iter()
            .enumerate()
            .map(|(i, &t)| (None, format!("${}", i + 1), Some(t))),
    );
    let types = super::pipeline::typed_on(e, &columns, release).ok()?;
    types.get(&(e as *const Expr)).copied()
}

/// The text a function's body and DEFAULTs are written in.
pub(crate) fn written_text(f: &Function) -> String {
    let defaults: Vec<String> = f.defaults.iter().map(|d| d.to_string()).collect();
    format!("{} {}", f.statement, defaults.join(" "))
}

/// Whether a statement's text names a function of the file whose body or DEFAULTs name text the
/// test `names` finds, itself or through another function of the file it names.
pub(crate) fn names_through(catalog: &crate::catalog::Catalog, sql: &str, names: fn(&str) -> bool) -> bool {
    let bare = |f: &Function| f.name.rsplit('.').next().unwrap_or(&f.name).to_ascii_lowercase();
    let texts: Vec<(String, String)> = catalog
        .functions
        .iter()
        .map(|f| (bare(f), written_text(f).to_ascii_lowercase()))
        .collect();
    let mut found: Vec<bool> = texts.iter().map(|(_, t)| names(t)).collect();
    loop {
        let more: Vec<usize> = (0..texts.len())
            .filter(|&i| !found[i])
            .filter(|&i| (0..texts.len()).any(|j| found[j] && texts[i].1.contains(&texts[j].0)))
            .collect();
        if more.is_empty() {
            break;
        }
        for i in more {
            found[i] = true;
        }
    }
    let lower = sql.to_ascii_lowercase();
    texts.iter().zip(&found).any(|((n, _), f)| *f && lower.contains(n.as_str()))
}

/// A parameter's type as PostgreSQL holds it: without its modifier.
pub(crate) fn parameter_type(t: ColumnType) -> ColumnType {
    super::typed::unmodified(t)
}

/// Whether the statement under way holds any function of the file's.
fn holds_any() -> bool {
    OWN.with(|o| !o.borrow().is_empty())
}

/// The function a call names, by PostgreSQL's search path: a name in a schema names that schema's;
/// a bare name public's, where pg_catalog holds no function of the name.
fn found(called: &str) -> Option<Rc<Own>> {
    OWN.with(|o| {
        let o = o.borrow();
        if o.is_empty() {
            return None;
        }
        let (schema, last) = match called.rsplit_once('.') {
            Some((s, l)) => (Some(s), l),
            None => (None, called),
        };
        o.iter()
            .find(|g| {
                let (gs, gl) = match g.name.rsplit_once('.') {
                    Some((s, l)) => (s, l),
                    None => ("public", g.name.as_str()),
                };
                gl == last
                    && match schema {
                        Some(s) => s == gs,
                        None => gs == "public" && !built_in_name(last),
                    }
            })
            .cloned()
    })
}

/// Whether pg_catalog holds a function of the name.
pub(super) fn built_in_name(name: &str) -> bool {
    static NAMES: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    NAMES
        .get_or_init(|| crate::functions::all().iter().map(|f| f.name).collect())
        .contains(name)
}

/// A call's arguments as written: each its name where it is given by one, and its expression.
type Arguments<'a> = Vec<(Option<String>, &'a Expr)>;

/// A call's arguments as written, each with its name where it is given by name; refused as
/// PostgreSQL's parser refuses a name given twice or a place given after a name. None where an
/// argument is written in a form a call of the file's does not read.
fn written(f: &sqlparser::ast::Function) -> Result<Option<Arguments<'_>>> {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
    let list = match &f.args {
        FunctionArguments::None => return Ok(Some(Vec::new())),
        FunctionArguments::List(l) => &l.args,
        FunctionArguments::Subquery(_) => return Ok(None),
    };
    let mut out: Vec<(Option<String>, &Expr)> = Vec::with_capacity(list.len());
    for a in list {
        let (name, x) = match a {
            FunctionArg::Unnamed(FunctionArgExpr::Expr(x)) => (None, x),
            FunctionArg::ExprNamed {
                name: Expr::Identifier(i),
                arg: FunctionArgExpr::Expr(x),
                operator: sqlparser::ast::FunctionArgOperator::RightArrow,
            }
            | FunctionArg::Named {
                name: i,
                arg: FunctionArgExpr::Expr(x),
                operator: sqlparser::ast::FunctionArgOperator::RightArrow,
            } => (Some(crate::catalog::ident(i)), x),
            _ => return Ok(None),
        };
        match &name {
            Some(n) if out.iter().any(|(m, _)| m.as_deref() == Some(n)) => {
                return Err(Error::new(format!("argument name \"{n}\" used more than once")))
            }
            None if out.iter().any(|(m, _)| m.is_some()) => {
                return Err(Error::new("positional argument cannot follow named argument"))
            }
            _ => {}
        }
        out.push((name, x));
    }
    Ok(Some(out))
}

/// Where each of a function's parameters takes its value in a call, as PostgreSQL matches a call
/// to a function: the arguments by place, then each by its name at a place none took, every
/// parameter left a DEFAULT, each argument's type one its parameter takes implicitly. None where
/// the call does not match.
fn matched(g: &Own, written: &[(Option<String>, Option<ColumnType>)]) -> Option<Vec<Given>> {
    if written.len() > g.args.len() {
        return None;
    }
    let mut slots: Vec<Option<Given>> = vec![None; g.args.len()];
    for (i, (name, _)) in written.iter().enumerate() {
        let at = match name {
            None => i,
            Some(n) => g.names.iter().position(|x| x.as_deref() == Some(n.as_str()))?,
        };
        if slots[at].is_some() {
            return None;
        }
        slots[at] = Some(Given::Arg(i));
    }
    let required = g.args.len() - g.defaults.len();
    let slots = slots
        .into_iter()
        .enumerate()
        .map(|(at, s)| s.or_else(|| (at >= required).then(|| Given::Default(at - required))))
        .collect::<Option<Vec<Given>>>()?;
    let fits = slots.iter().zip(&g.args).all(|(s, to)| match s {
        Given::Arg(i) => super::handed::implicit(written[*i].1, *to).is_some(),
        Given::Default(_) => true,
    });
    fits.then_some(slots)
}

/// `function f(a, b => c) does not exist`, each argument by its type, a named one after its name
/// as written.
fn no_form(called: &str, written: &[(Option<String>, Option<ColumnType>)]) -> Error {
    let shown: Vec<String> = written
        .iter()
        .map(|(n, t)| match n {
            Some(n) => format!("{n} => {}", super::typed::arg_name(*t)),
            None => super::typed::arg_name(*t),
        })
        .collect();
    Error::new(format!(
        "function {called}({}) does not exist{}",
        shown.join(", "),
        super::typed::NO_FUNCTION
    ))
}

/// The type a call of a function of the file's returns, read before any row; none where the call
/// names none.
pub(super) fn typed(
    f: &sqlparser::ast::Function,
    type_of: &mut dyn FnMut(&Expr) -> Result<Option<ColumnType>>,
) -> Result<Option<ColumnType>> {
    if !holds_any() {
        return Ok(None);
    }
    let called = crate::catalog::object_name(&f.name);
    let g = found(&called);
    // A bare name only a function of the file's outside public has: off the search path.
    let elsewhere = g.is_none()
        && !called.contains('.')
        && !built_in_name(&called)
        && OWN.with(|o| {
            o.borrow()
                .iter()
                .any(|g| g.name.rsplit('.').next() == Some(called.as_str()))
        });
    if g.is_none() && !elsewhere {
        return Ok(None);
    }
    let Some(args) = written(f)? else {
        return Err(Error::new(format!("{f}: its arguments are not read")));
    };
    let mut typed = Vec::with_capacity(args.len());
    for (name, x) in &args {
        typed.push((name.clone(), type_of(x)?));
    }
    let Some(g) = g.filter(|g| matched(g, &typed).is_some()) else {
        return Err(no_form(&called, &typed));
    };
    if f.over.is_some() {
        return Err(Error::new(format!(
            "OVER specified, but {called} is not a window function nor an aggregate function"
        )));
    }
    if f.filter.is_some() {
        return Err(Error::new(format!(
            "FILTER specified, but {called} is not an aggregate function"
        )));
    }
    if !f.within_group.is_empty() {
        return Err(Error::new(format!(
            "WITHIN GROUP specified, but {called} is not an aggregate function"
        )));
    }
    if let sqlparser::ast::FunctionArguments::List(l) = &f.args {
        if l.duplicate_treatment.is_some() {
            return Err(Error::new(format!(
                "DISTINCT specified, but {called} is not an aggregate function"
            )));
        }
        if !l.clauses.is_empty() {
            return Err(Error::new(format!(
                "ORDER BY specified, but {called} is not an aggregate function"
            )));
        }
    }
    Ok(Some(g.returns))
}

/// Whether a statement calls a function of the file's.
pub(crate) fn calls(catalog: &crate::catalog::Catalog, q: &sqlparser::ast::Query) -> bool {
    calls_in(catalog, q)
}

/// Whether anything a statement holds calls a function of the file's.
pub(crate) fn calls_in<V: sqlparser::ast::Visit>(catalog: &crate::catalog::Catalog, q: &V) -> bool {
    if catalog.functions.is_empty() {
        return false;
    }
    let named = |called: &str| {
        let (schema, last) = match called.rsplit_once('.') {
            Some((s, l)) => (Some(s), l),
            None => (None, called),
        };
        catalog.functions.iter().any(|g| {
            let (gs, gl) = match g.name.rsplit_once('.') {
                Some((s, l)) => (s, l),
                None => ("public", g.name.as_str()),
            };
            gl == last && schema.is_none_or(|s| s == gs)
        })
    };
    sqlparser::ast::visit_expressions(q, |e| match e {
        Expr::Function(f) if named(&crate::catalog::object_name(&f.name)) => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

/// Whether anything a statement holds calls a function of the file's whose body reads relations,
/// itself or through a function of the file its body or its DEFAULTs call: such a call asks the
/// snail its body's statement.
pub(crate) fn reads_relations<V: sqlparser::ast::Visit>(
    catalog: &crate::catalog::Catalog,
    q: &V,
) -> bool {
    !catalog.functions.is_empty() && calls_reading(catalog, q, &mut Vec::new())
}

fn calls_reading<V: sqlparser::ast::Visit>(
    catalog: &crate::catalog::Catalog,
    q: &V,
    seen: &mut Vec<String>,
) -> bool {
    sqlparser::ast::visit_expressions(q, |e| match e {
        Expr::Function(f) => match named_in(catalog, &crate::catalog::object_name(&f.name)) {
            Some(g) if reading(catalog, g, seen) => ControlFlow::Break(()),
            _ => ControlFlow::Continue(()),
        },
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

fn reading(catalog: &crate::catalog::Catalog, g: &Function, seen: &mut Vec<String>) -> bool {
    if seen.contains(&g.name) {
        return false;
    }
    seen.push(g.name.clone());
    let sublink = sqlparser::ast::visit_expressions(&g.body, |e| match e {
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => ControlFlow::Break(()),
        _ => ControlFlow::Continue(()),
    });
    sublink.is_break()
        || calls_reading(catalog, &g.body, seen)
        || g.defaults.iter().any(|d| calls_reading(catalog, d, seen))
}

/// The function of a catalog's a call names, as `found` finds one of those held.
fn named_in<'c>(
    catalog: &'c crate::catalog::Catalog,
    called: &str,
) -> Option<&'c Function> {
    let (schema, last) = match called.rsplit_once('.') {
        Some((s, l)) => (Some(s), l),
        None => (None, called),
    };
    catalog.functions.iter().find(|g| {
        let (gs, gl) = match g.name.rsplit_once('.') {
            Some((s, l)) => (s, l),
            None => ("public", g.name.as_str()),
        };
        gl == last
            && match schema {
                Some(s) => s == gs,
                None => gs == "public" && !built_in_name(last),
            }
    })
}

/// The function of the file's an expression calls whose statement reads `table` from a VOLATILE
/// function: one VOLATILE itself, or called by one, whose body names the table. Such a statement,
/// in a CHECK or a DEFAULT, reads the rows its write made before the row it is called for.
pub(crate) fn reads_written(
    catalog: &crate::catalog::Catalog,
    e: &Expr,
    table: &str,
) -> Option<String> {
    if catalog.functions.is_empty() {
        return None;
    }
    let mut found = None;
    let _ = sqlparser::ast::visit_expressions(e, |x| {
        let Expr::Function(f) = x else {
            return ControlFlow::Continue(());
        };
        match named_in(catalog, &crate::catalog::object_name(&f.name)) {
            Some(g) if reads_table(catalog, g, table, false, &mut Vec::new()) => {
                found = Some(g.name.clone());
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        }
    });
    found
}

/// Whether a function's body, or a body of the file's functions it calls, names `table` under a
/// VOLATILE function: `volatile` where a call around it is one.
fn reads_table(
    catalog: &crate::catalog::Catalog,
    g: &Function,
    table: &str,
    volatile: bool,
    seen: &mut Vec<(String, bool)>,
) -> bool {
    let volatile = volatile || g.volatility == 'v';
    if seen.contains(&(g.name.clone(), volatile)) {
        return false;
    }
    seen.push((g.name.clone(), volatile));
    let full = crate::oids::full(table);
    let names = volatile
        && sqlparser::ast::visit_relations(&g.body, |n| {
            match crate::oids::full(&crate::catalog::object_name(n)) == full {
                true => ControlFlow::Break(()),
                false => ControlFlow::Continue(()),
            }
        })
        .is_break();
    names
        || std::iter::once(&g.body).chain(&g.defaults).any(|x| {
            sqlparser::ast::visit_expressions(x, |e| match e {
                Expr::Function(f) => match named_in(catalog, &crate::catalog::object_name(&f.name)) {
                    Some(h) if reads_table(catalog, h, table, volatile, seen) => ControlFlow::Break(()),
                    _ => ControlFlow::Continue(()),
                },
                _ => ControlFlow::Continue(()),
            })
            .is_break()
        })
}

/// A call's value on a row; none where the call names no function of the file's.
pub(super) fn call(
    f: &sqlparser::ast::Function,
    row: &[Value],
    frame: &Frame,
    computed: &Computed,
) -> Result<Option<Value>> {
    if !holds_any() {
        return Ok(None);
    }
    let called = crate::catalog::object_name(&f.name);
    let Some(g) = found(&called) else {
        return Ok(None);
    };
    if let Some(why) = &g.refused {
        return Err(Error::new(format!("{called}: {why}")));
    }
    let _deeper = deeper()?;
    let Some(args) = written(f)? else {
        return Err(Error::new(format!("{f}: its arguments are not read")));
    };
    let typed: Vec<(Option<String>, Option<ColumnType>)> =
        args.iter().map(|(n, x)| (n.clone(), frame.type_of(x))).collect();
    let Some(slots) = matched(&g, &typed) else {
        return Err(no_form(&called, &typed));
    };
    // Each parameter's expression, read on the call's row where the call gives it, or its DEFAULT
    // read over no row.
    let casings = Casings::default();
    let frames: Vec<Frame> = g
        .defaults
        .iter()
        .map(|d| Frame {
            columns: Vec::new(),
            params: &[],
            release: frame.release,
            casings: &casings,
            types: &d.types,
            now: frame.now,
            subqueries: None,
            reads: frame.reads,
            below: None,
        })
        .collect();
    let expr_at = |at: usize| -> &Expr {
        match slots[at] {
            Given::Arg(i) => args[i].1,
            Given::Default(k) => &g.defaults[k].expr,
        }
    };
    let ev = |at: usize| match slots[at] {
        Given::Arg(i) => super::pipeline::eval(args[i].1, row, frame, computed),
        Given::Default(k) => super::pipeline::eval(&g.defaults[k].expr, &[], &frames[k], computed),
    };
    let fit = |at: usize, v: Value| {
        super::handed::fitted(v, g.args[at], super::pipeline::quoted(expr_at(at)))
    };
    let props_at = |at: usize| match slots[at] {
        Given::Arg(i) => props(args[i].1, &|x| frame.type_of(x), frame.release),
        Given::Default(k) => {
            let d = &g.defaults[k];
            props(&d.expr, &|x| d.types.get(&(x as *const Expr)).copied(), frame.release)
        }
    };
    let inlined = inlined(&g)
        && (0..slots.len()).all(|at| {
            g.uses[at] <= 1 || {
                let p = props_at(at);
                !p.sublink && p.volatility < 2 && p.cost <= 10.0
            }
        });
    let given: RefCell<Vec<Option<Value>>> = RefCell::new(vec![None; slots.len()]);
    if !inlined {
        for at in 0..slots.len() {
            let v = fit(at, ev(at)?)?;
            given.borrow_mut()[at] = Some(v);
        }
        if g.strict && given.borrow().iter().any(|v| matches!(v, Some(Value::Null))) {
            return Ok(Some(Value::Null));
        }
    }
    if let Some(text) = &g.question {
        let values: Vec<Value> =
            given.into_inner().into_iter().map(|v| v.unwrap_or(Value::Null)).collect();
        let v = asked(&g, text, &values, frame)?;
        return super::handed::fitted(v, g.returns, false).map(Some);
    }
    let bound = |e: &Expr| -> Option<Result<Value>> {
        let Expr::Identifier(i) = e else {
            return None;
        };
        if i.quote_style != Some('"') {
            return None;
        }
        let at = i.value.strip_prefix('$')?.parse::<usize>().ok()?.checked_sub(1)?;
        if at >= slots.len() {
            return None;
        }
        if let Some(v) = given.borrow()[at].clone() {
            return Some(Ok(v));
        }
        let v = ev(at).and_then(|v| fit(at, v));
        if let Ok(v) = &v {
            given.borrow_mut()[at] = Some(v.clone());
        }
        Some(v)
    };
    let casings = Casings::default();
    let body_frame = Frame {
        columns: (1..=g.args.len()).map(|n| (None, format!("${n}"))).collect(),
        params: &[],
        release: frame.release,
        casings: &casings,
        types: &g.types,
        now: frame.now,
        subqueries: None,
        reads: frame.reads,
        below: None,
    };
    let none = vec![Value::Null; g.args.len()];
    let v = super::pipeline::eval(&g.body, &none, &body_frame, &Computed::bound_by(&bound))?;
    super::handed::fitted(v, g.returns, false).map(Some)
}

/// A body that reads relations, called as 18.6 calls a SQL function it does not inline: its
/// statement planned with each argument's value as a constant, as a custom plan takes it, its
/// constants folded; then run through the snail the caller reads, which answers with its first row,
/// NULL where it makes none.
fn asked(g: &Own, text: &str, values: &[Value], frame: &Frame) -> Result<Value> {
    use sqlparser::ast::Statement;
    let Some(reads) = frame.reads else {
        return Err(Error::new(format!(
            "{}: a body that reads relations is not called here",
            g.name
        )));
    };
    // Where a question's shape is found over tables holding no rows, nothing is asked of them.
    if !reads.holds_rows() {
        return Ok(Value::Null);
    }
    let catalog = reads.catalog();
    let scanned = super::spelled::scanned(text, catalog.release)?;
    let spelled = super::spelled::spelled(&scanned)?;
    let parsed = super::image::parse_sql(&spelled)?;
    let [Statement::Query(q)] = parsed.as_slice() else {
        return Err(Error::new(format!("{}: its body is not one SELECT", g.name)));
    };
    let mut q = (**q).clone();
    let given = values
        .iter()
        .zip(&g.args)
        .map(|(v, &t)| constant(v, t))
        .collect::<Result<Vec<Expr>>>()?;
    let relations = relations_of(catalog);
    parameters_named(&mut q, &g.name, &g.names, Some(&relations), &|n| given[n - 1].clone());
    // One row is fetched of it: where it says no more, it is told so.
    if q.limit_clause.is_none() && q.fetch.is_none() {
        q.limit_clause = Some(sqlparser::ast::LimitClause::LimitOffset {
            limit: Some(super::image::parse_expr("1")?),
            offset: None,
            limit_by: Vec::new(),
        });
    }
    super::folded::folded(catalog, &q.to_string(), Some(reads))?;
    let q = super::pipeline::windows::written(&super::spelled::between_written(&q))?;
    let q = super::computed(catalog, &q)?.unwrap_or(q);
    let row = super::relations::first_row(reads, &q)?;
    Ok(row.and_then(|r| r.into_iter().next()).unwrap_or(Value::Null))
}

/// An argument's value written as a constant of its parameter's type.
fn constant(v: &Value, t: ColumnType) -> Result<Expr> {
    let written = match v.text() {
        Some(s) => format!("'{}'", s.replace('\'', "''")),
        None => "NULL".into(),
    };
    Ok(super::image::parse_expr(&format!(
        "CAST({written} AS {})",
        t.pg_name()
    ))?)
}

/// The calls of the file's functions inside one another taken before the next is refused, as 18.6
/// takes `c4.down(8422)` and refuses `c4.down(8423)` under its default `max_stack_depth` (2048kB).
const CALLS: usize = 8423;

/// The stack the calls may take from the outermost, on a thread whose host gave none.
const STACK: usize = 1 << 20;

/// What a thread's stack keeps past the calls' own: the deepest call's reading, and what its host
/// stands on below the stack it gave.
const MARGIN: usize = 1 << 20;

/// PostgreSQL's refusal of a call past its stack.
const TOO_DEEP: &str = "stack depth limit exceeded\nHINT:  Increase the configuration parameter \
     \"max_stack_depth\" (currently 2048kB), after ensuring the platform's stack depth limit is \
     adequate.";

thread_local! {
    /// Where the outermost call under way on this thread stands on the stack, and how many calls
    /// are under way.
    static BASE: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
    /// Where this thread's stack begins and how large its host gave it, where it said.
    static GIVEN: std::cell::Cell<Option<(usize, usize)>> = const { std::cell::Cell::new(None) };
}

/// Says that the thread this is called on was given a stack of `bytes`, from about where it is
/// called: calls of a snail's functions inside one another then take up to it, less a mebibyte,
/// before the next is refused. A thread told nothing gives them a mebibyte from the outermost.
/// Either way they are refused past PostgreSQL's own count, as it refuses them under its default
/// `max_stack_depth`.
pub fn stack_given(bytes: usize) {
    let here = 0u8;
    let at = std::ptr::addr_of!(here) as usize;
    GIVEN.with(|g| g.set(Some((at, bytes))));
}

/// A call under way: the outermost lets its place on the stack go where it is dropped.
struct Deeper;

impl Drop for Deeper {
    fn drop(&mut self) {
        BASE.with(|b| {
            b.set(match b.get() {
                Some((base, n)) if n > 1 => Some((base, n - 1)),
                _ => None,
            })
        });
    }
}

fn deeper() -> Result<Deeper> {
    let here = 0u8;
    let at = std::ptr::addr_of!(here) as usize;
    let (base, n) = BASE
        .with(|b| b.get())
        .map_or((at, 1), |(base, n)| (base, n + 1));
    let within = match GIVEN.with(|g| g.get()) {
        Some((top, bytes)) => top.abs_diff(at) <= bytes.saturating_sub(MARGIN),
        None => base.abs_diff(at) <= STACK,
    };
    if n > CALLS || !within {
        return Err(Error::new(TOO_DEEP));
    }
    BASE.with(|b| b.set(Some((base, n))));
    Ok(Deeper)
}

/// Whether 18.6 inlines a call of the file's function of this name before its arguments are looked
/// at, and how often its body names each parameter; none where the file holds no such function or
/// its body is not read.
pub(crate) fn inlining(called: &str) -> Option<(bool, Vec<usize>)> {
    let g = found(called)?;
    if g.refused.is_some() {
        return None;
    }
    Some((inlined(&g), g.uses.clone()))
}

/// Whether 18.6 inlines a function, before its arguments are looked at: a body of one expression
/// holding no subquery and nothing that folds rows, no more volatile than the function says, and
/// for a STRICT one strict throughout and naming every parameter.
fn inlined(g: &Own) -> bool {
    let p = g.held;
    !p.sublink
        && !p.folds
        && match g.volatility {
            'i' => p.volatility == 0,
            's' => p.volatility <= 1,
            _ => true,
        }
        && (!g.strict || (!p.nonstrict && g.uses.iter().all(|&u| u > 0)))
}

/// A row of the carried pg_proc: what the planner reads of a function.
#[derive(Clone, Debug)]
struct Proc {
    args: Vec<u32>,
    strict: bool,
    volatility: u8,
    cost: f64,
    kind: char,
    retset: bool,
}

/// The carried pg_proc, pg_operator, pg_cast and pg_type of a release, as the walk reads them.
struct Carried {
    by_name: HashMap<String, Vec<Proc>>,
    by_oid: HashMap<u32, Proc>,
    /// Each operator by its name: its left and right types' OIDs and its function's.
    operators: HashMap<String, Vec<(u32, u32, u32)>>,
    /// Each cast by its source and target: its function and method.
    casts: HashMap<(u32, u32), (u32, char)>,
    /// Each type's input and output functions.
    io: HashMap<u32, (u32, u32)>,
}

fn carried(release: u8) -> Arc<Carried> {
    static ALL: OnceLock<Mutex<HashMap<u8, Arc<Carried>>>> = OnceLock::new();
    let all = ALL.get_or_init(|| Mutex::new(HashMap::new()));
    let mut all = all.lock().unwrap_or_else(|e| e.into_inner());
    all.entry(release)
        .or_insert_with(|| {
            let field = |r: &HashMap<&'static str, Option<String>>, c: &str| r.get(c).cloned().flatten().unwrap_or_default();
            let number = |s: String| s.parse::<u32>().unwrap_or(0);
            let mut by_name: HashMap<String, Vec<Proc>> = HashMap::new();
            let mut by_oid = HashMap::new();
            for r in crate::pgcatalog::built_in("pg_proc", release) {
                let p = Proc {
                    args: field(&r, "proargtypes")
                        .split_whitespace()
                        .map(|a| a.parse().unwrap_or(0))
                        .collect(),
                    strict: field(&r, "proisstrict") == "t",
                    volatility: match field(&r, "provolatile").as_str() {
                        "i" => 0,
                        "s" => 1,
                        _ => 2,
                    },
                    cost: field(&r, "procost").parse().unwrap_or(1.0),
                    kind: field(&r, "prokind").chars().next().unwrap_or('f'),
                    retset: field(&r, "proretset") == "t",
                };
                by_oid.insert(number(field(&r, "oid")), p.clone());
                by_name.entry(field(&r, "proname")).or_default().push(p);
            }
            let mut operators: HashMap<String, Vec<(u32, u32, u32)>> = HashMap::new();
            for r in crate::pgcatalog::built_in("pg_operator", release) {
                operators.entry(field(&r, "oprname")).or_default().push((
                    number(field(&r, "oprleft")),
                    number(field(&r, "oprright")),
                    number(field(&r, "oprcode")),
                ));
            }
            let mut casts = HashMap::new();
            for r in crate::pgcatalog::built_in("pg_cast", release) {
                casts.insert(
                    (number(field(&r, "castsource")), number(field(&r, "casttarget"))),
                    (
                        number(field(&r, "castfunc")),
                        field(&r, "castmethod").chars().next().unwrap_or('f'),
                    ),
                );
            }
            let mut io = HashMap::new();
            for r in crate::pgcatalog::built_in("pg_type", release) {
                io.insert(
                    number(field(&r, "oid")),
                    (number(field(&r, "typinput")), number(field(&r, "typoutput"))),
                );
            }
            Arc::new(Carried {
                by_name,
                by_oid,
                operators,
                casts,
                io,
            })
        })
        .clone()
}

/// Whether every function of a name the release's catalog holds returns a set.
pub(crate) fn returns_set(name: &str, release: u8) -> bool {
    carried(release)
        .by_name
        .get(name)
        .is_some_and(|all| !all.is_empty() && all.iter().all(|p| p.retset))
}

/// A type's OID under a release, where it is a built-in one.
fn oid_of(t: Option<ColumnType>, release: u8) -> Option<u32> {
    crate::regs::type_oid(&t?.pg_name(), release).ok()
}

/// What a function holds: of the one whose arguments' types are the call's, where the types are
/// known and one is; otherwise of every one of the name and the number of arguments, taken at
/// their worst.
fn of_procs(c: &Carried, name: &str, args: &[Option<u32>]) -> Option<Props> {
    let all: Vec<&Proc> = c
        .by_name
        .get(name)?
        .iter()
        .filter(|p| p.args.len() == args.len())
        .collect();
    let exact: Vec<&Proc> = all
        .iter()
        .copied()
        .filter(|p| p.args.iter().zip(args).all(|(a, b)| b.is_none_or(|b| b == *a)))
        .collect();
    let chosen = if exact.is_empty() { all } else { exact };
    if chosen.is_empty() {
        return None;
    }
    let mut out = Props::default();
    for p in chosen {
        out.volatility = out.volatility.max(p.volatility);
        out.nonstrict |= !p.strict;
        out.cost = out.cost.max(p.cost);
        out.folds |= p.kind == 'a' || p.kind == 'w' || p.retset;
    }
    Some(out)
}

fn of_proc(p: &Proc) -> Props {
    Props {
        volatility: p.volatility,
        nonstrict: !p.strict,
        cost: p.cost,
        sublink: false,
        folds: p.kind == 'a' || p.kind == 'w' || p.retset,
    }
}

/// An operator's function, by its name and its operands' types.
fn of_operator(c: &Carried, name: &str, left: Option<u32>, right: Option<u32>, prefix: bool) -> Props {
    let Some(all) = c.operators.get(name) else {
        return Props {
            cost: 1.0,
            ..Props::default()
        };
    };
    let fits = |(l, r, _): &&(u32, u32, u32)| {
        (if prefix { *l == 0 } else { left.is_none_or(|x| x == *l) }) && right.is_none_or(|x| x == *r)
    };
    let exact: Vec<&(u32, u32, u32)> = all.iter().filter(fits).collect();
    let shaped: Vec<&(u32, u32, u32)> = all.iter().filter(|(l, _, _)| (*l == 0) == prefix).collect();
    let chosen = if exact.is_empty() { shaped } else { exact };
    let mut out = Props {
        cost: 1.0,
        ..Props::default()
    };
    for (_, _, code) in chosen {
        if let Some(p) = c.by_oid.get(code) {
            out.volatility = out.volatility.max(p.volatility);
            out.nonstrict |= !p.strict;
            out.cost = out.cost.max(p.cost);
        }
    }
    out
}

/// A cast's function: its own, or the types' output and input where it goes through text.
fn of_cast(c: &Carried, from: Option<u32>, to: Option<u32>) -> Props {
    let (Some(from), Some(to)) = (from, to) else {
        return Props::default();
    };
    if from == to {
        return Props::default();
    }
    let io = |c: &Carried| {
        let mut out = Props::default();
        for f in [c.io.get(&from).map(|x| x.1), c.io.get(&to).map(|x| x.0)].into_iter().flatten() {
            if let Some(p) = c.by_oid.get(&f) {
                out.volatility = out.volatility.max(p.volatility);
                out.cost += p.cost;
            }
        }
        out
    };
    match c.casts.get(&(from, to)) {
        Some((func, 'f')) => c.by_oid.get(func).map(of_proc).unwrap_or_default(),
        Some((_, 'i')) => io(c),
        Some(_) => Props::default(),
        None => io(c),
    }
}

/// What an expression holds, as 18.6's planner reads it: its walkers of what is mutable, what is
/// not strict and what an argument costs.
fn props(e: &Expr, type_of: &dyn Fn(&Expr) -> Option<ColumnType>, release: u8) -> Props {
    let c = carried(release);
    let mut out = Props::default();
    walk(e, type_of, release, &c, &mut out);
    out
}

fn walk(e: &Expr, type_of: &dyn Fn(&Expr) -> Option<ColumnType>, release: u8, c: &Carried, out: &mut Props) {
    let oid = |x: &Expr| oid_of(type_of(x), release);
    let nonstrict = Props {
        nonstrict: true,
        ..Props::default()
    };
    let go = |x: &Expr, out: &mut Props| walk(x, type_of, release, c, out);
    match e {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Value(_)
        | Expr::TypedString { .. }
        | Expr::Interval(_) => {}
        Expr::Nested(x) | Expr::Collate { expr: x, .. } => go(x, out),
        Expr::Cast { expr, data_type, .. } => {
            // A literal cast is a constant, made as the statement is read.
            if !matches!(expr.as_ref(), Expr::Value(_)) {
                let to = crate::regs::type_oid(&data_type.to_string(), release).ok();
                out.with(of_cast(c, oid(expr), to));
                go(expr, out);
            }
        }
        Expr::BinaryOp { left, op, right } => {
            match op {
                BinaryOperator::And | BinaryOperator::Or => out.with(nonstrict),
                _ => {
                    let name = match op {
                        BinaryOperator::PGCustomBinaryOperator(parts) => {
                            parts.last().cloned().unwrap_or_default()
                        }
                        other => other.to_string(),
                    };
                    out.with(of_operator(c, &name, oid(left), oid(right), false));
                }
            }
            go(left, out);
            go(right, out);
        }
        Expr::UnaryOp { op, expr } => {
            if !matches!(op, UnaryOperator::Not) {
                out.with(of_operator(c, &op.to_string(), None, oid(expr), true));
            }
            go(expr, out);
        }
        Expr::IsNull(x)
        | Expr::IsNotNull(x)
        | Expr::IsTrue(x)
        | Expr::IsNotTrue(x)
        | Expr::IsFalse(x)
        | Expr::IsNotFalse(x)
        | Expr::IsUnknown(x)
        | Expr::IsNotUnknown(x) => {
            out.with(nonstrict);
            go(x, out);
        }
        Expr::IsDistinctFrom(a, b) | Expr::IsNotDistinctFrom(a, b) => {
            out.with(nonstrict);
            out.with(of_operator(c, "=", oid(a), oid(b), false));
            go(a, out);
            go(b, out);
        }
        Expr::Between { expr, low, high, .. } => {
            out.with(nonstrict);
            for x in [expr, low, high] {
                out.with(of_operator(c, "<=", oid(expr), oid(x), false));
                go(x, out);
            }
        }
        Expr::InList { expr, list, .. } => {
            let op = of_operator(c, "=", oid(expr), list.first().and_then(&oid), false);
            out.with(Props {
                cost: op.cost * (list.len() as f64 * 0.5).max(1.0),
                nonstrict: op.nonstrict || list.is_empty(),
                ..op
            });
            go(expr, out);
            for x in list {
                go(x, out);
            }
        }
        Expr::AnyOp { left, right, .. } | Expr::AllOp { left, right, .. } => {
            // Strict only over an array written out with elements, provably not empty.
            let written = matches!(right.as_ref(), Expr::Array(a) if !a.elem.is_empty());
            out.with(Props {
                nonstrict: !written,
                cost: 1.0,
                ..Props::default()
            });
            go(left, out);
            go(right, out);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            out.with(nonstrict);
            if let Some(x) = operand {
                go(x, out);
            }
            for w in conditions {
                go(&w.condition, out);
                go(&w.result, out);
            }
            if let Some(x) = else_result {
                go(x, out);
            }
        }
        Expr::Array(a) => {
            out.with(nonstrict);
            for x in &a.elem {
                go(x, out);
            }
        }
        Expr::Tuple(xs) => {
            out.with(nonstrict);
            for x in xs {
                go(x, out);
            }
        }
        Expr::Subquery(_) | Expr::Exists { .. } | Expr::InSubquery { .. } => {
            out.with(Props {
                nonstrict: true,
                sublink: true,
                ..Props::default()
            });
        }
        Expr::AtTimeZone {
            timestamp,
            time_zone,
        } => {
            let zone = oid(time_zone).or_else(|| oid_of(Some(ColumnType::Text(None)), release));
            if let Some(p) = of_procs(c, "timezone", &[zone, oid(timestamp)]) {
                out.with(p);
            }
            go(timestamp, out);
            go(time_zone, out);
        }
        Expr::Extract { expr, .. } => {
            let text = oid_of(Some(ColumnType::Text(None)), release);
            if let Some(p) = of_procs(c, "extract", &[text, oid(expr)]) {
                out.with(p);
            }
            go(expr, out);
        }
        Expr::Function(f) => {
            let called = crate::catalog::object_name(&f.name);
            let name = called.rsplit('.').next().unwrap_or(&called).to_string();
            let args: Vec<&Expr> = match &f.args {
                sqlparser::ast::FunctionArguments::List(l) => l
                    .args
                    .iter()
                    .filter_map(|a| match a {
                        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(x))
                        | sqlparser::ast::FunctionArg::Named {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(x),
                            ..
                        }
                        | sqlparser::ast::FunctionArg::ExprNamed {
                            arg: sqlparser::ast::FunctionArgExpr::Expr(x),
                            ..
                        } => Some(x),
                        _ => None,
                    })
                    .collect(),
                _ => Vec::new(),
            };
            if f.over.is_some() {
                out.with(Props {
                    folds: true,
                    nonstrict: true,
                    ..Props::default()
                });
            } else if let Some(g) = found(&called) {
                // The DEFAULTs a call leaves to the function are its arguments too.
                for d in g.defaults.iter().skip(args.len().saturating_sub(g.args.len() - g.defaults.len())) {
                    out.with(props(&d.expr, &|x| d.types.get(&(x as *const Expr)).copied(), release));
                }
                // Another of the file's: as its planner reads it once it is inlined, or its call.
                if inlined(&g) {
                    out.with(g.held);
                } else {
                    out.with(Props {
                        volatility: match g.volatility {
                            'i' => 0,
                            's' => 1,
                            _ => 2,
                        },
                        nonstrict: !g.strict,
                        cost: 100.0,
                        ..Props::default()
                    });
                }
            } else {
                match name.as_str() {
                    "coalesce" | "nullif" | "greatest" | "least" => out.with(Props {
                        nonstrict: true,
                        cost: if name == "nullif" { 1.0 } else { 0.0 },
                        ..Props::default()
                    }),
                    "current_date" | "current_time" | "current_timestamp" | "localtime"
                    | "localtimestamp" | "current_user" | "current_role" | "session_user"
                    | "user" | "current_catalog" | "current_schema"
                        if matches!(f.args, sqlparser::ast::FunctionArguments::None) =>
                    {
                        out.with(Props {
                            volatility: 1,
                            ..Props::default()
                        })
                    }
                    _ => {
                        let types: Vec<Option<u32>> = args.iter().map(|x| oid(x)).collect();
                        match of_procs(c, &name, &types) {
                            Some(p) => out.with(p),
                            // Neither the snail's nor the release's: read at its worst.
                            None => out.with(Props {
                                nonstrict: true,
                                volatility: 2,
                                cost: 1.0,
                                ..Props::default()
                            }),
                        }
                    }
                }
            }
            for x in args {
                go(x, out);
            }
        }
        Expr::Like { expr, pattern, .. }
        | Expr::ILike { expr, pattern, .. }
        | Expr::SimilarTo { expr, pattern, .. } => {
            out.with(Props {
                cost: 1.0,
                ..Props::default()
            });
            go(expr, out);
            go(pattern, out);
        }
        Expr::Ceil { expr, .. } | Expr::Floor { expr, .. } => {
            out.with(Props {
                cost: 1.0,
                ..Props::default()
            });
            go(expr, out);
        }
        Expr::Substring {
            expr,
            substring_from,
            substring_for,
            ..
        } => {
            out.with(Props {
                cost: 1.0,
                ..Props::default()
            });
            go(expr, out);
            for x in [substring_from, substring_for].into_iter().flatten() {
                go(x, out);
            }
        }
        Expr::Trim { expr, trim_what, .. } => {
            out.with(Props {
                cost: 1.0,
                ..Props::default()
            });
            go(expr, out);
            if let Some(x) = trim_what {
                go(x, out);
            }
        }
        Expr::Position { expr, r#in } => {
            out.with(Props {
                cost: 1.0,
                ..Props::default()
            });
            go(expr, out);
            go(r#in, out);
        }
        Expr::CompoundFieldAccess { root, .. } => go(root, out),
        // Anything else read at its worst for strictness: the call, not the inlining.
        _ => out.with(Props {
            nonstrict: true,
            cost: 1.0,
            ..Props::default()
        }),
    }
}
