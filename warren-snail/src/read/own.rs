//! The functions a snail's own DDL declares, called wherever a statement calls them, as PostgreSQL
//! 18.6 calls a function written in SQL. Its planner inlines one where it can, and then the body
//! reads each argument where it reaches it, a CASE's branch not taken reading none. Otherwise it
//! calls it: each argument computed once, left to right, and a STRICT function answering NULL for a
//! NULL argument without its body. Which of the two, it decides from the body and the arguments, by
//! the strictness, volatility and cost its catalog gives each function, operator and cast they
//! hold; those are read here from the catalog the snail carries for the file's release.

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
    /// Why it is not called, where its body is not read.
    refused: Option<String>,
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
                "{}\u{1}{:?}\u{1}{:?}\u{1}{}{}\u{1}{}",
                f.name, f.args, f.returns, f.strict, f.volatility, f.source
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
    let all = made_all(functions, release);
    OWN.with(|o| *o.borrow_mut() = all.clone());
    MADE.with(|m| *m.borrow_mut() = Some((key, all)));
    held
}

/// Every function made ready, in the DDL's order. Each is held by its signature first, so a body
/// may call any of them, itself among them; each body is then typed with those before it made.
fn made_all(functions: &[Function], release: u8) -> Rc<Vec<Rc<Own>>> {
    let stubs: Vec<Rc<Own>> = functions.iter().map(|f| Rc::new(stub(f))).collect();
    OWN.with(|o| *o.borrow_mut() = Rc::new(stubs));
    for (i, f) in functions.iter().enumerate() {
        let own = Rc::new(made(f, release));
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
        returns: f.returns,
        strict: f.strict,
        volatility: f.volatility,
        body: Box::new(f.body.clone()),
        types: Default::default(),
        uses: vec![1; f.args.len()],
        held: Props::default(),
        refused: Some("its body is not made yet".into()),
    }
}

/// Whether a body names a column or a function the function cannot read, refused in PostgreSQL's
/// words: a name no parameter has, a relation the body holds none of, a function neither the DDL
/// nor pg_catalog holds.
fn unknown_name(body: &Expr, types: &super::typed::Types, release: u8) -> Option<String> {
    let mut why = None;
    let c = carried(release);
    let _ = sqlparser::ast::visit_expressions(body, |e| {
        let found = match e {
            Expr::Identifier(i) if !(i.quote_style == Some('"') && i.value.starts_with('$')) => {
                Some(format!("column \"{}\" does not exist", crate::catalog::ident(i)))
            }
            Expr::CompoundIdentifier(parts) if parts.len() >= 2 => Some(format!(
                "missing FROM-clause entry for table \"{}\"",
                crate::catalog::ident(&parts[parts.len() - 2])
            )),
            Expr::Function(f) => {
                let called = crate::catalog::object_name(&f.name);
                let bare = called.strip_prefix("pg_catalog.").unwrap_or(&called);
                let known = found(&called).is_some()
                    || !bare.contains('.')
                        && (c.by_name.contains_key(bare) || SYNTAX.contains(&bare));
                (!known).then(|| {
                    let args: Vec<Option<ColumnType>> = match &f.args {
                        sqlparser::ast::FunctionArguments::List(l) => l
                            .args
                            .iter()
                            .map(|a| match a {
                                sqlparser::ast::FunctionArg::Unnamed(
                                    sqlparser::ast::FunctionArgExpr::Expr(x),
                                ) => types.get(&(x as *const Expr)).copied(),
                                _ => None,
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    super::typed::no_form(&called, &args).to_string()
                })
            }
            _ => None,
        };
        match found {
            Some(w) => {
                why = Some(w);
                ControlFlow::Break(())
            }
            None => ControlFlow::Continue(()),
        }
    });
    why
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

/// A function made ready: its parameters named as columns, its body typed with them.
fn made(f: &Function, release: u8) -> Own {
    let mut body = Box::new(f.body.clone());
    let mut uses = vec![0; f.args.len()];
    let mut beyond = None;
    let _ = sqlparser::ast::visit_expressions_mut(body.as_mut(), |e| {
        if let Expr::Value(v) = e {
            if let sqlparser::ast::Value::Placeholder(p) = &v.value {
                match p.strip_prefix('$').and_then(|n| n.parse::<usize>().ok()) {
                    Some(n) if n >= 1 && n <= uses.len() => {
                        uses[n - 1] += 1;
                        *e = Expr::Identifier(sqlparser::ast::Ident::with_quote('"', p.clone()));
                    }
                    _ => {
                        beyond.get_or_insert(format!("there is no parameter {p}"));
                    }
                }
            }
        }
        ControlFlow::<()>::Continue(())
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
                let why = unknown_name(&body, &t, release);
                (t, why)
            }
            Err(e) => (Default::default(), Some(e.to_string())),
        },
    };
    let held = props(&body, &|x| types.get(&(x as *const Expr)).copied(), release);
    Own {
        name: f.name.clone(),
        args: f.args.clone(),
        returns: f.returns,
        strict: f.strict,
        volatility: f.volatility,
        body,
        types,
        uses,
        held,
        refused,
    }
}

/// The type a function's body gives, typed as 18.6 types it when the function is made, after
/// `before` and itself: refused where its parser refuses the body. A quoted literal alone is text.
pub(crate) fn body_type(
    before: &[Function],
    f: &Function,
    release: u8,
) -> Result<Option<ColumnType>> {
    let all: Vec<Function> = before.iter().cloned().chain([f.clone()]).collect();
    let _held = holding(&[], release);
    let made = made_all(&all, release);
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
        .or_else(|| super::pipeline::quoted(body).then_some(ColumnType::Text(None))))
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
fn built_in_name(name: &str) -> bool {
    static NAMES: OnceLock<std::collections::HashSet<&'static str>> = OnceLock::new();
    NAMES
        .get_or_init(|| crate::functions::all().iter().map(|f| f.name).collect())
        .contains(name)
}

/// Whether a function of the file's takes a call's arguments, as PostgreSQL's implicit casts take
/// them; refused in its words where it does not.
fn taken(g: &Own, called: &str, types: &[Option<ColumnType>]) -> Result<()> {
    if g.args.len() == types.len()
        && types
            .iter()
            .zip(&g.args)
            .all(|(from, to)| super::handed::implicit(*from, *to).is_some())
    {
        return Ok(());
    }
    Err(super::typed::no_form(called, types))
}

/// The type a call of a function of the file's returns, read before any row; none where the call
/// names none.
pub(super) fn typed(
    f: &sqlparser::ast::Function,
    types: &[Option<ColumnType>],
) -> Result<Option<ColumnType>> {
    if !holds_any() {
        return Ok(None);
    }
    let called = crate::catalog::object_name(&f.name);
    let Some(g) = found(&called) else {
        // A bare name only a function of the file's outside public has: off the search path.
        let elsewhere = !called.contains('.')
            && !built_in_name(&called)
            && OWN.with(|o| {
                o.borrow()
                    .iter()
                    .any(|g| g.name.rsplit('.').next() == Some(called.as_str()))
            });
        if elsewhere {
            return Err(super::typed::no_form(&called, types));
        }
        return Ok(None);
    };
    taken(&g, &called, types)?;
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

/// A call's value on a row; none where the call names no function of the file's.
pub(super) fn call(
    f: &sqlparser::ast::Function,
    args: &[&Expr],
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
    let types: Vec<Option<ColumnType>> = args.iter().map(|x| frame.type_of(x)).collect();
    taken(&g, &called, &types)?;
    let ev = |x: &Expr| super::pipeline::eval(x, row, frame, computed);
    let fit = |i: usize, v: Value| super::handed::fitted(v, g.args[i], super::pipeline::quoted(args[i]));
    let inlined = inlined(&g)
        && args.iter().zip(&g.uses).all(|(a, &u)| {
            u <= 1 || {
                let p = props(a, &|x| frame.type_of(x), frame.release);
                !p.sublink && p.volatility < 2 && p.cost <= 10.0
            }
        });
    let given: RefCell<Vec<Option<Value>>> = RefCell::new(vec![None; args.len()]);
    if !inlined {
        for (i, a) in args.iter().enumerate() {
            let v = fit(i, ev(a)?)?;
            given.borrow_mut()[i] = Some(v);
        }
        if g.strict && given.borrow().iter().any(|v| matches!(v, Some(Value::Null))) {
            return Ok(Some(Value::Null));
        }
    }
    let bound = |e: &Expr| -> Option<Result<Value>> {
        let Expr::Identifier(i) = e else {
            return None;
        };
        if i.quote_style != Some('"') {
            return None;
        }
        let at = i.value.strip_prefix('$')?.parse::<usize>().ok()?.checked_sub(1)?;
        if at >= args.len() {
            return None;
        }
        if let Some(v) = given.borrow()[at].clone() {
            return Some(Ok(v));
        }
        let v = ev(args[at]).and_then(|v| fit(at, v));
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
    };
    let none = vec![Value::Null; g.args.len()];
    let v = super::pipeline::eval(&g.body, &none, &body_frame, &Computed::bound_by(&bound))?;
    super::handed::fitted(v, g.returns, false).map(Some)
}

/// The stack calls of the file's functions inside one another may take, from the outermost, before
/// the next is refused, as PostgreSQL refuses a call past its `max_stack_depth`.
const STACK: usize = 1 << 20;

thread_local! {
    /// Where the outermost call under way on this thread stands on the stack.
    static BASE: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

/// A call under way: the outermost lets its place on the stack go where it is dropped.
struct Deeper(bool);

impl Drop for Deeper {
    fn drop(&mut self) {
        if self.0 {
            BASE.with(|b| b.set(None));
        }
    }
}

fn deeper() -> Result<Deeper> {
    let here = 0u8;
    let at = std::ptr::addr_of!(here) as usize;
    BASE.with(|b| match b.get() {
        None => {
            b.set(Some(at));
            Ok(Deeper(true))
        }
        Some(base) if base.abs_diff(at) > STACK => Err(Error::new("stack depth limit exceeded")),
        Some(_) => Ok(Deeper(false)),
    })
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
