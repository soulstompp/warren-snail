//! The functions a caller hands a snail, called from SQL by their names as any function is: scalar,
//! aggregate and set-returning, each of the types its signature names, a row of several columns
//! among them. The pipeline's typing reads each signature before any row: a call no form takes is
//! refused as PostgreSQL refuses one, and each argument reaches the function in the type its
//! signature names.
//!
//! An aggregate's rows reach it in the order its own ORDER BY gives, rows that tie in the order the
//! step reads them; with no ORDER BY, in the order the step reads its rows: over one table, the
//! table's order; over several relations, none promised.

use super::pipeline::Value;
use crate::catalog::ColumnType;
use crate::composed::{Composed, Field, Item};
use crate::{Error, Result};
use std::cell::RefCell;
use std::rc::Rc;

/// An aggregate as it folds: one row's arguments at a time, then its value.
pub trait Fold {
    /// Folds in one row's arguments, each in the type the signature names, NULLs among them.
    fn add(&mut self, args: &[Value]) -> Result<()>;
    /// The aggregate's value over the rows folded in, of the type the signature returns.
    fn finish(&mut self) -> Result<Value>;
}

type Scalar = dyn Fn(&[Value]) -> Result<Value>;
type Set = dyn Fn(&[Value]) -> Result<Vec<Value>>;
type Start = dyn Fn() -> Box<dyn Fold>;

/// What a function does with its arguments' values.
enum Body {
    Scalar(Box<Scalar>),
    Aggregate(Box<Start>),
    Set(Box<Set>),
}

/// A function a caller hands a snail: its name, the types of its arguments, the type it returns,
/// and what it computes. It is called on every row, NULLs among its arguments, as PostgreSQL calls
/// a function declared without STRICT.
pub struct Function {
    name: String,
    args: Vec<ColumnType>,
    returns: ColumnType,
    body: Body,
}

impl std::fmt::Debug for Function {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.signature())
    }
}

impl Function {
    /// A function giving one value for each call, `returns` a row type where it gives a row.
    pub fn scalar(
        name: &str,
        args: &[ColumnType],
        returns: ColumnType,
        body: impl Fn(&[Value]) -> Result<Value> + 'static,
    ) -> Function {
        Function::of(name, args, returns, Body::Scalar(Box::new(body)))
    }

    /// An aggregate: `start` makes the fold of each group, which takes the group's rows and gives
    /// its value.
    pub fn aggregate(
        name: &str,
        args: &[ColumnType],
        returns: ColumnType,
        start: impl Fn() -> Box<dyn Fold> + 'static,
    ) -> Function {
        Function::of(name, args, returns, Body::Aggregate(Box::new(start)))
    }

    /// A set-returning function: the values each call gives, one a row, each of the type
    /// `returns`, a row type where it gives rows of several columns.
    pub fn set_returning(
        name: &str,
        args: &[ColumnType],
        returns: ColumnType,
        body: impl Fn(&[Value]) -> Result<Vec<Value>> + 'static,
    ) -> Function {
        Function::of(name, args, returns, Body::Set(Box::new(body)))
    }

    fn of(name: &str, args: &[ColumnType], returns: ColumnType, body: Body) -> Function {
        Function {
            name: name.to_string(),
            args: args.to_vec(),
            returns,
            body,
        }
    }

    /// Its name, as a call writes it: unquoted where it is lower case.
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn args(&self) -> &[ColumnType] {
        &self.args
    }

    pub fn returns(&self) -> ColumnType {
        self.returns
    }

    /// Its name and its arguments' types, as PostgreSQL's errors write a signature.
    pub fn signature(&self) -> String {
        format!(
            "{}({})",
            self.name,
            self.args
                .iter()
                .map(|t| t.pg_name())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }

    fn kind(&self) -> &'static str {
        match self.body {
            Body::Scalar(_) => "a scalar function",
            Body::Aggregate(_) => "an aggregate",
            Body::Set(_) => "a set-returning function",
        }
    }

    /// The last part of its name, as a call unqualified names it.
    fn last(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or(&self.name)
    }
}

/// The type of a row of named columns: a composite named `name`, a field each column.
pub fn row_type(name: &str, columns: &[(&str, ColumnType)]) -> ColumnType {
    crate::composed::type_of(Composed::Record {
        name: name.to_string(),
        fields: columns.iter().map(|(n, t)| Field::new(n, *t)).collect(),
    })
}

/// The type of an array of `element`.
pub fn array_type(element: ColumnType) -> ColumnType {
    super::typed::array_of(element)
}

/// A type by its name in SQL, as a cast writes it: `bigint`, `text[]`, a composite or a range a DDL
/// this process read declares.
pub fn type_named(sql: &str) -> Result<ColumnType> {
    let dt = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
        .try_with_sql(sql)?
        .parse_data_type()?;
    crate::catalog::held_type(&dt)
        .ok()
        .or_else(|| crate::composed::resolve(&dt))
        .ok_or_else(|| Error::new(format!("type \"{sql}\" does not exist")))
}

impl Value {
    /// A row of the row type `ty`: each value assigned to its column's type.
    pub fn row(ty: ColumnType, values: Vec<Value>) -> Result<Value> {
        let Some(fields) = fields_of(ty) else {
            return Err(Error::new(format!("{} is not a row type", ty.pg_name())));
        };
        if fields.len() != values.len() {
            return Err(Error::new(format!(
                "a row of {} has {} columns, not {}",
                ty.pg_name(),
                fields.len(),
                values.len()
            )));
        }
        let items = fields
            .iter()
            .zip(values)
            .map(|(f, v)| super::pipeline::part_of(f.ty, v, false))
            .collect::<Result<Vec<_>>>()?;
        Ok(super::pipeline::composed(ty, Item::Record(items)))
    }

    /// An array of the array type `ty`, of one dimension from 1: each value assigned to its
    /// element's type.
    pub fn array(ty: ColumnType, values: Vec<Value>) -> Result<Value> {
        let Some(element) = super::typed::element_of(ty) else {
            return Err(Error::new(format!("{} is not an array type", ty.pg_name())));
        };
        let n = values.len() as i32;
        let elements = values
            .into_iter()
            .map(|v| super::pipeline::part_of(element, v, false))
            .collect::<Result<Vec<_>>>()?;
        Ok(super::pipeline::composed(
            ty,
            Item::Array {
                elements,
                dims: if n == 0 { Vec::new() } else { vec![n] },
                lower: if n == 0 { Vec::new() } else { vec![1] },
            },
        ))
    }

    /// A row's values, each of its column's type; none where it is no row.
    pub fn fields(&self) -> Option<Vec<Value>> {
        let Value::Composed(ty, c) = self else {
            return None;
        };
        let fields = fields_of(*ty)?;
        match &c.item {
            Item::Record(items) => Some(
                fields
                    .iter()
                    .zip(items)
                    .map(|(f, i)| super::pipeline::part_value(f.ty, i))
                    .collect(),
            ),
            _ => None,
        }
    }

    /// An array's elements in storage order, each of its element's type; none where it is no
    /// array.
    pub fn elements(&self) -> Option<Vec<Value>> {
        let Value::Composed(ty, c) = self else {
            return None;
        };
        let element = super::typed::element_of(*ty)?;
        match &c.item {
            Item::Array { elements, .. } => Some(
                elements
                    .iter()
                    .map(|i| super::pipeline::part_value(element, i))
                    .collect(),
            ),
            _ => None,
        }
    }
}

/// The fields of a row type.
fn fields_of(ty: ColumnType) -> Option<&'static [Field]> {
    match ty {
        ColumnType::Composed(r) => match r.def() {
            Composed::Record { fields, .. } => Some(fields),
            _ => None,
        },
        _ => None,
    }
}

/// The names PostgreSQL's grammar reads as its own syntax, which no function of the catalog takes.
const SYNTAX: [&str; 6] = ["coalesce", "greatest", "least", "nullif", "trim", "case"];

/// The functions a snail has been handed, checked as each is handed: refused where PostgreSQL's
/// catalog or the snail's DDL has a function of its name, where a form of the same arguments is
/// handed already, or where a function of another kind has its name.
pub(crate) fn hand(
    handed: &mut Rc<Vec<Rc<Function>>>,
    catalog: &crate::catalog::Catalog,
    f: Function,
) -> Result<()> {
    let refused = |why: String| Err(Error::new(format!("{}: {why}", f.signature())));
    let parts: Vec<&str> = f.name.split('.').collect();
    if f.name.is_empty() || parts.iter().any(|p| p.is_empty()) || parts.len() > 2 {
        return refused("a function is named by a name, or a schema and a name".into());
    }
    let last = f.last();
    if SYNTAX.contains(&last) || crate::functions::all().iter().any(|g| g.name == last) {
        return refused(format!(
            "{last} is PostgreSQL's own: a function handed in takes a name of its own"
        ));
    }
    if catalog
        .functions
        .iter()
        .any(|g| g.name == f.name || g.name.rsplit('.').next() == Some(last))
    {
        return refused(format!("the snail's DDL declares a function {last}"));
    }
    if matches!(f.body, Body::Aggregate(_)) && f.args.is_empty() {
        return refused("an aggregate handed in takes one argument or more".into());
    }
    for g in handed.iter().filter(|g| g.last() == last) {
        if g.kind() != f.kind() {
            return refused(format!("{} is handed in already as {}", g.name, g.kind()));
        }
        if g.name == f.name && g.args == f.args {
            return Err(Error::new(format!(
                "function {} already exists with same argument types",
                f.name
            )));
        }
    }
    let mut all = (**handed).clone();
    all.push(Rc::new(f));
    *handed = Rc::new(all);
    Ok(())
}

/// The tables of a snail, each by its name with its columns: their row types.
type Tables = Rc<Vec<(String, Vec<Field>)>>;

thread_local! {
    /// The functions the round under way on this thread was handed.
    static HANDED: RefCell<Rc<Vec<Rc<Function>>>> = RefCell::new(Rc::new(Vec::new()));
    /// The tables of the snail the round under way reads.
    static ROWS: RefCell<Tables> = RefCell::new(Rc::new(Vec::new()));
}

/// The functions a round is handed, read by all it evaluates while it is held; those before come
/// back where it is dropped.
pub(crate) struct Holding(Rc<Vec<Rc<Function>>>);

/// Holds a round's functions.
pub(crate) fn holding(handed: &Rc<Vec<Rc<Function>>>) -> Holding {
    Holding(HANDED.with(|h| h.replace(handed.clone())))
}

impl Drop for Holding {
    fn drop(&mut self) {
        let before = std::mem::take(&mut self.0);
        HANDED.with(|h| *h.borrow_mut() = before);
    }
}

/// The row types a pipeline reads its tables' names as, held while it runs.
pub(crate) struct Rows(Tables);

/// Holds the row types of a catalog's tables.
pub(crate) fn rows_of(catalog: &crate::catalog::Catalog) -> Rows {
    let rows = catalog
        .tables
        .iter()
        .map(|t| {
            let fields = t
                .columns
                .iter()
                .map(|c| Field {
                    name: c.name.clone(),
                    ty: c.ty,
                    domain: c.domain.clone(),
                })
                .collect();
            (t.name.clone(), fields)
        })
        .collect();
    Rows(ROWS.with(|r| r.replace(Rc::new(rows))))
}

impl Drop for Rows {
    fn drop(&mut self) {
        let before = std::mem::take(&mut self.0);
        ROWS.with(|r| *r.borrow_mut() = before);
    }
}

/// A table's row type, by the table's name or the last part of it, where the pipeline under way
/// reads a snail holding one table of the name.
pub(crate) fn table_row(name: &str) -> Option<ColumnType> {
    ROWS.with(|r| {
        let r = r.borrow();
        let mut found = r
            .iter()
            .filter(|(n, _)| n == name || n.rsplit('.').next() == Some(name));
        let (n, fields) = found.next()?;
        if found.next().is_some() {
            return None;
        }
        Some(crate::composed::type_of(Composed::Record {
            name: n.clone(),
            fields: fields.clone(),
        }))
    })
}

/// Whether a name is that of a function handed in of the kind `body` gives.
fn handed_as(name: &str, body: fn(&Body) -> bool) -> bool {
    HANDED.with(|h| {
        h.borrow()
            .iter()
            .any(|f| f.last() == name && body(&f.body))
    })
}

/// Whether a name is a handed aggregate's.
pub(super) fn is_aggregate(name: &str) -> bool {
    handed_as(name, |b| matches!(b, Body::Aggregate(_)))
}

/// Whether a name is a handed set-returning function's.
pub(super) fn is_set_returning(name: &str) -> bool {
    handed_as(name, |b| matches!(b, Body::Set(_)))
}

/// Whether a call names a function handed in.
pub(super) fn is_handed(f: &sqlparser::ast::Function) -> bool {
    let called = crate::catalog::object_name(&f.name);
    HANDED.with(|h| h.borrow().iter().any(|g| named(g, &called)))
}

/// Whether a call's name names a handed function: its whole name, or the last part alone.
fn named(f: &Function, called: &str) -> bool {
    called == f.name || called == f.last()
}

/// Where an argument of type `from` is taken as one of type `to` with no cast written, as
/// PostgreSQL's implicit casts take it: whether exactly, else whether at all. A literal of no type
/// yet, or NULL, is taken as any type.
fn implicit(from: Option<ColumnType>, to: ColumnType) -> Option<bool> {
    use ColumnType as C;
    let Some(from) = from else {
        return Some(false);
    };
    let base = |t: ColumnType| match t {
        C::Text(Some(_)) => C::Text(None),
        C::Bpchar(_) => C::Bpchar(None),
        t if t.is_numeric() => super::typed::NUMERIC,
        t => super::clock::base(t),
    };
    if base(from) == base(to) {
        return Some(true);
    }
    let place = |t: ColumnType| -> Option<(u8, u8)> {
        Some(match t {
            C::Int2 => (0, 0),
            C::Int4 => (0, 1),
            C::Int8 => (0, 2),
            t if t.is_numeric() => (0, 3),
            C::Float4 => (0, 4),
            C::Float8 => (0, 5),
            C::Text(_) | C::Bpchar(_) | C::Name => (1, 0),
            C::Date => (2, 0),
            C::Timestamp(_) => (2, 1),
            C::Timestamptz | C::TimestamptzRounded(_) => (2, 2),
            C::Time(_) => (3, 0),
            C::Timetz(_) => (3, 1),
            _ => return None,
        })
    };
    match (place(from), place(to)) {
        (Some((a, x)), Some((b, y))) if a == b && (x < y || a == 1) => Some(false),
        _ => None,
    }
}

/// The handed function a call names that takes its arguments' types: of its forms, the one taking
/// most of them exactly. None where no function of the name is handed; PostgreSQL's refusal where
/// none of its forms takes them, or more than one takes them as well.
fn chosen(
    f: &sqlparser::ast::Function,
    types: &[Option<ColumnType>],
) -> Result<Option<Rc<Function>>> {
    let called = crate::catalog::object_name(&f.name);
    HANDED.with(|h| {
        let h = h.borrow();
        let of_name: Vec<&Rc<Function>> = h.iter().filter(|g| named(g, &called)).collect();
        if of_name.is_empty() {
            return Ok(None);
        }
        let mut best: Vec<(&Rc<Function>, usize)> = Vec::new();
        for g in of_name.iter().filter(|g| g.args.len() == types.len()) {
            let taken: Option<Vec<bool>> = types
                .iter()
                .zip(&g.args)
                .map(|(from, to)| implicit(*from, *to))
                .collect();
            if let Some(taken) = taken {
                best.push((g, taken.iter().filter(|x| **x).count()));
            }
        }
        let most = best.iter().map(|(_, n)| *n).max();
        best.retain(|(_, n)| Some(*n) == most);
        // At an argument of no type yet, the forms taking a string there, where any does.
        for (i, _) in types.iter().enumerate().filter(|(_, t)| t.is_none()) {
            let string = |g: &Function| {
                matches!(
                    g.args[i],
                    ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name
                )
            };
            if best.iter().any(|(g, _)| string(g)) {
                best.retain(|(g, _)| string(g));
            }
        }
        match best.as_slice() {
            [(g, _)] => Ok(Some(Rc::clone(g))),
            [] => Err(super::typed::no_form(&called, types)),
            _ => Err(Error::new(format!(
                "function {called}({}) is not unique",
                types
                    .iter()
                    .map(|t| t.map_or("unknown".into(), |t| t.pg_name()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    })
}

/// The type a call of a handed function returns, read before any row; none where the call names no
/// function handed in. Refused where it is not called as its kind is: an aggregate or a
/// set-returning function over a window, a scalar one with OVER, WITHIN GROUP or FILTER.
pub(super) fn typed(
    f: &sqlparser::ast::Function,
    types: &[Option<ColumnType>],
) -> Result<Option<ColumnType>> {
    let Some(g) = chosen(f, types)? else {
        return Ok(None);
    };
    let name = crate::catalog::object_name(&f.name);
    match g.body {
        Body::Aggregate(_) if f.over.is_some() => Err(Error::new(format!(
            "{name} is an aggregate handed in: one over a window is not read"
        ))),
        Body::Aggregate(_) if !f.within_group.is_empty() => Err(Error::new(format!(
            "WITHIN GROUP is specified, but {name} is not an ordered-set aggregate"
        ))),
        Body::Scalar(_) | Body::Set(_) if f.over.is_some() => Err(Error::new(format!(
            "OVER specified, but {name} is not a window function nor an aggregate function"
        ))),
        Body::Scalar(_) | Body::Set(_) if f.filter.is_some() => Err(Error::new(format!(
            "FILTER specified, but {name} is not an aggregate function"
        ))),
        _ => Ok(Some(g.returns)),
    }
}

/// Each argument's value in the type the function's signature names.
fn taken(g: &Function, given: Vec<(Value, bool)>) -> Result<Vec<Value>> {
    given
        .into_iter()
        .zip(&g.args)
        .map(|((v, quoted), ty)| fitted(v, *ty, quoted))
        .collect()
}

/// A value in a type: a literal of no type yet read by the type's input, any other assigned to it.
fn fitted(v: Value, ty: ColumnType, quoted: bool) -> Result<Value> {
    Ok(super::pipeline::part_value(
        ty,
        &super::pipeline::part_of(ty, v, quoted)?,
    ))
}

/// What a function gave, in the type it returns; refused, naming the function, where it is not one.
fn returned(g: &Function, v: Value) -> Result<Value> {
    fitted(v, g.returns, false).map_err(|e| {
        Error::new(format!(
            "{} returned a value that is no {}: {e}",
            g.signature(),
            g.returns.pg_name()
        ))
    })
}

/// A scalar call's value; none where the call names no scalar function handed in.
pub(super) fn call(
    f: &sqlparser::ast::Function,
    given: Vec<(Value, bool)>,
    types: &[Option<ColumnType>],
) -> Result<Option<Value>> {
    let Some(g) = chosen(f, types)? else {
        return Ok(None);
    };
    let Body::Scalar(body) = &g.body else {
        return Ok(None);
    };
    let args = taken(&g, given)?;
    Ok(Some(returned(&g, body(&args)?)?))
}

/// A set-returning call's values, one a row; none where the call names no set-returning function
/// handed in.
pub(super) fn rows(
    f: &sqlparser::ast::Function,
    given: Vec<(Value, bool)>,
    types: &[Option<ColumnType>],
) -> Result<Option<Vec<Value>>> {
    let Some(g) = chosen(f, types)? else {
        return Ok(None);
    };
    let Body::Set(body) = &g.body else {
        return Ok(None);
    };
    let args = taken(&g, given)?;
    body(&args)?
        .into_iter()
        .map(|v| returned(&g, v))
        .collect::<Result<Vec<_>>>()
        .map(Some)
}

/// The handed aggregate a call names, as it starts to fold; none where it names none.
pub(super) fn aggregate(
    f: &sqlparser::ast::Function,
    types: &[Option<ColumnType>],
) -> Result<Option<Rc<Function>>> {
    Ok(chosen(f, types)?.filter(|g| matches!(g.body, Body::Aggregate(_))))
}

/// A row's arguments as a handed aggregate folds them: in the types its signature names.
pub(super) fn fold_row(g: &Function, args: &[Value]) -> Result<Vec<Value>> {
    taken(g, args.iter().cloned().map(|v| (v, false)).collect())
}

/// A handed aggregate's value over its rows, folded in the order given.
pub(super) fn folded(g: &Function, rows: &[Vec<Value>]) -> Result<Value> {
    let Body::Aggregate(start) = &g.body else {
        unreachable!("an aggregate")
    };
    let mut fold = start();
    for r in rows {
        fold.add(r)?;
    }
    returned(g, fold.finish()?)
}
