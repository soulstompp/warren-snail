//! A question over several relations, read as a pipeline: each step's FROM met as one relation, and
//! each step then read as the pipeline reads a step over one. The tables of a FROM the question path
//! joins are read as C1's round meets them, one round; any other table is read alone, by the
//! conditions of its own the question path reads; and the relations a FROM names (tables, steps, a
//! derived table, VALUES or a view lifted to a step of its own, a function's rows) are met by their
//! ON, USING or NATURAL, a function's rows made for each row of the relations before it.

use super::collate::Carried;
use super::pipeline::{self, Casings, Columns, Relation, Value, MERGED};
use super::question::{self, Question};
use super::typed::{self, TypedColumn};
use super::Round;
use crate::catalog::{Catalog, ColumnType, Table};
use crate::{Error, Result};
use arrow::array::RecordBatch;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, JoinConstraint,
    JoinOperator, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, TableAlias, TableFactor,
    TableWithJoins,
};
use std::collections::HashMap;

/// Where the tables a question reads are read: one snail, or several opened together.
pub(super) trait Reads {
    /// The catalog naming the tables.
    fn catalog(&self) -> &Catalog;
    /// A question the question path reads, over the catalog's tables: one round, each snail's part
    /// of it in its `snails` where there are several.
    fn read(&self, q: &Question, order: Option<&str>) -> Result<(RecordBatch, Round)>;
    /// How many snails the tables lie in.
    fn snails(&self) -> usize;
}

/// The mark a function's place in a FROM is named by, once lifted out of it.
const CALLED: &str = "\0call";

/// A relation the statement makes before its last SELECT, under the name its FROM reads it by: a
/// WITH query, or a derived table, a VALUES list or a view lifted out of a FROM; last, the
/// question itself, under no name.
struct Step {
    name: String,
    query: Query,
    /// The names an alias gives its columns, in place of their own.
    renames: Vec<String>,
    /// The functions its FROM calls, each standing in its FROM under CALLED and its place here.
    calls: Vec<Call>,
}

/// A function a FROM reads the rows of.
#[derive(Clone)]
struct Call {
    expr: Expr,
    ordinality: bool,
}

/// What a step made: its rows, and the type and the collation of each of its columns.
struct Made {
    rel: Relation,
    types: Vec<Option<ColumnType>>,
    carried: Vec<Carried>,
}

/// The steps a question makes, in the order they are read: each WITH query, each relation lifted
/// out of a FROM before the step that reads it, and the question last.
fn steps(catalog: &Catalog, q: &Query) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let mut visible: Vec<String> = Vec::new();
    if let Some(with) = &q.with {
        if with.recursive {
            return Err(Error::new("WITH RECURSIVE is not read"));
        }
        for cte in &with.cte_tables {
            let name = crate::catalog::ident(&cte.alias.name);
            let renames = cte
                .alias
                .columns
                .iter()
                .map(|c| crate::catalog::ident(&c.name))
                .collect();
            lift(
                catalog,
                &mut out,
                &visible,
                name.clone(),
                (*cte.query).clone(),
                renames,
            )?;
            visible.push(name);
        }
    }
    let mut last = q.clone();
    last.with = None;
    lift(catalog, &mut out, &visible, String::new(), last, Vec::new())?;
    Ok(out)
}

/// One step, after the relations its FROM lifts out of itself.
fn lift(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    name: String,
    mut query: Query,
    renames: Vec<String>,
) -> Result<()> {
    if query.with.is_some() {
        return Err(Error::new("a WITH inside a step is not read"));
    }
    let mut calls = Vec::new();
    match query.body.as_mut() {
        SetExpr::Select(s) => {
            for item in &mut s.from {
                lift_join(catalog, out, visible, item, &mut calls)?;
            }
        }
        SetExpr::Values(_) => {
            if query.order_by.is_some() || query.limit_clause.is_some() || query.fetch.is_some() {
                return Err(Error::new("a VALUES list sorted or limited is not read"));
            }
        }
        SetExpr::SetOperation { .. } => return Err(Error::new("set operations are not read")),
        other => {
            return Err(Error::new(format!(
                "{other} is not read: a step is a SELECT"
            )))
        }
    }
    out.push(Step {
        name,
        query,
        renames,
        calls,
    });
    Ok(())
}

fn lift_join(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    item: &mut TableWithJoins,
    calls: &mut Vec<Call>,
) -> Result<()> {
    lift_factor(catalog, out, visible, &mut item.relation, calls)?;
    for j in &mut item.joins {
        lift_factor(catalog, out, visible, &mut j.relation, calls)?;
    }
    Ok(())
}

/// A name of its own for a relation lifted out of a FROM.
fn fresh(out: &[Step], what: &str) -> String {
    format!("{MERGED}{what}{}", out.len())
}

fn named(n: &str) -> ObjectName {
    ObjectName(vec![ObjectNamePart::Identifier(Ident::with_quote('"', n))])
}

fn alias_named(n: &str) -> TableAlias {
    TableAlias {
        explicit: true,
        name: Ident::with_quote('"', n),
        columns: Vec::new(),
        at: None,
    }
}

/// The names an alias gives a relation's columns.
fn renames_of(alias: &Option<TableAlias>) -> Result<Vec<String>> {
    let Some(a) = alias else {
        return Ok(Vec::new());
    };
    a.columns
        .iter()
        .map(|c| match &c.data_type {
            None => Ok(crate::catalog::ident(&c.name)),
            Some(_) => Err(Error::new(format!(
                "{a}: a column definition list is not read"
            ))),
        })
        .collect()
}

/// A FROM item as the steps read it: a table or a step by name; a derived table, a VALUES list and
/// a view lifted out to a step of their own; a function's call kept beside the step.
fn lift_factor(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    t: &mut TableFactor,
    calls: &mut Vec<Call>,
) -> Result<()> {
    let call = |name: &ObjectName, args: Vec<FunctionArg>| {
        Expr::Function(sqlparser::ast::Function {
            name: name.clone(),
            uses_odbc_syntax: false,
            parameters: FunctionArguments::None,
            args: FunctionArguments::List(sqlparser::ast::FunctionArgumentList {
                duplicate_treatment: None,
                args,
                clauses: Vec::new(),
            }),
            filter: None,
            null_treatment: None,
            over: None,
            within_group: Vec::new(),
        })
    };
    let (expr, ordinality, alias) = match t {
        TableFactor::Table {
            name,
            alias,
            args: None,
            with_hints,
            version: None,
            partitions,
            json_path: None,
            sample: None,
            index_hints,
            ..
        } if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() => {
            let n = crate::catalog::object_name(name);
            if visible.contains(&n) {
                return Ok(());
            }
            if let Some(v) = catalog.view(&n) {
                let parsed = sqlparser::parser::Parser::parse_sql(
                    &sqlparser::dialect::PostgreSqlDialect {},
                    &v.query,
                )?;
                let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
                    return Err(Error::new(format!("the view {} is no question", v.name)));
                };
                let step = fresh(out, "view");
                lift(
                    catalog,
                    out,
                    &[],
                    step.clone(),
                    (**q).clone(),
                    v.columns.clone(),
                )?;
                let bare = v.name.rsplit('.').next().unwrap_or(&v.name).to_string();
                let kept = alias.take().unwrap_or_else(|| alias_named(&bare));
                *t = plain(&step, Some(kept));
            }
            return Ok(());
        }
        TableFactor::Table {
            name,
            alias,
            args: Some(a),
            with_ordinality,
            ..
        } if a.settings.is_none() => (call(name, a.args.clone()), *with_ordinality, alias.take()),
        TableFactor::Function {
            name,
            args,
            with_ordinality,
            alias,
            ..
        } => (call(name, args.clone()), *with_ordinality, alias.take()),
        TableFactor::UNNEST {
            alias,
            array_exprs,
            with_offset: false,
            with_offset_alias: None,
            with_ordinality,
        } => {
            if array_exprs.len() != 1 {
                return Err(Error::new("unnest of several arrays in FROM is not read"));
            }
            (
                call(
                    &named("unnest"),
                    vec![FunctionArg::Unnamed(FunctionArgExpr::Expr(
                        array_exprs[0].clone(),
                    ))],
                ),
                *with_ordinality,
                alias.take(),
            )
        }
        TableFactor::Derived {
            lateral: false,
            subquery,
            alias,
            sample: None,
        } => {
            let step = fresh(out, "derived");
            let renames = renames_of(alias)?;
            lift(
                catalog,
                out,
                visible,
                step.clone(),
                (**subquery).clone(),
                renames,
            )?;
            let kept = alias.take().map(|mut a| {
                a.columns.clear();
                a
            });
            *t = plain(&step, kept);
            return Ok(());
        }
        TableFactor::Derived { lateral: true, .. } => {
            return Err(Error::new(format!(
                "{t} is not read: a LATERAL subquery reads the rows before it"
            )))
        }
        TableFactor::NestedJoin {
            table_with_joins,
            alias: None,
        } => return lift_join(catalog, out, visible, table_with_joins, calls),
        other => return Err(Error::new(format!("{other} is not read in FROM"))),
    };
    calls.push(Call { expr, ordinality });
    *t = plain(&format!("{CALLED}{}", calls.len() - 1), alias);
    Ok(())
}

/// A FROM item naming a relation by name.
fn plain(name: &str, alias: Option<TableAlias>) -> TableFactor {
    TableFactor::Table {
        name: named(name),
        alias,
        args: None,
        with_hints: Vec::new(),
        version: None,
        with_ordinality: false,
        partitions: Vec::new(),
        json_path: None,
        sample: None,
        index_hints: Vec::new(),
    }
}

/// How a FROM meets two relations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Inner,
    Left,
    Right,
    Full,
    Cross,
}

/// A column of a FROM: the name of the relation it answers to, marked where a USING or NATURAL
/// join merged it away from a bare name, its own name, its type and the collation it carries.
#[derive(Clone, Debug)]
struct Col {
    qualifier: Option<String>,
    name: String,
    ty: Option<ColumnType>,
    carried: Carried,
}

/// A column a USING or NATURAL join makes of the two it merges, each by its place in the FROM's row.
#[derive(Clone, Debug)]
struct Merged {
    at: usize,
    left: usize,
    right: usize,
    ty: Option<ColumnType>,
}

/// One relation of a FROM, or two met: its columns lie at `at` in the FROM's row, `width` of them,
/// a join's its left side's, its right side's, then those it merges.
struct Node {
    what: What,
    at: usize,
    width: usize,
    /// The places a star reads it at.
    star: Vec<usize>,
}

enum What {
    /// A table of the snail, by its place in the catalog.
    Table { table: usize, qualifier: String },
    /// A step made before.
    Made { name: String, qualifier: String },
    /// A function's rows, made for each row of the relations before it.
    Call { call: Call, qualifier: String },
    Join {
        left: Box<Node>,
        right: Box<Node>,
        kind: Kind,
        on: Option<Expr>,
        merged: Vec<Merged>,
    },
}

impl Node {
    /// The relation a qualified star or a single name reads, where the node is one.
    fn leaf_named(&self) -> Option<&str> {
        match &self.what {
            What::Table { qualifier, .. }
            | What::Made { qualifier, .. }
            | What::Call { qualifier, .. } => Some(qualifier),
            What::Join { .. } => None,
        }
    }

    /// Each relation it reads, leftmost first.
    fn leaves<'n>(&'n self, out: &mut Vec<&'n Node>) {
        match &self.what {
            What::Join { left, right, .. } => {
                left.leaves(out);
                right.leaves(out);
            }
            _ => out.push(self),
        }
    }

    /// Whether its rows are made for each row before it: its leftmost relation is a function's.
    fn lateral(&self) -> bool {
        match &self.what {
            What::Call { .. } => true,
            What::Join { left, .. } => left.lateral(),
            _ => false,
        }
    }
}

/// A step's FROM, laid out: its columns in the row it makes, and its relations, each item after
/// the one before it.
struct From {
    columns: Vec<Col>,
    items: Vec<Node>,
}

impl From {
    fn typed(&self) -> Vec<TypedColumn> {
        self.columns
            .iter()
            .map(|c| (c.qualifier.clone(), c.name.clone(), c.ty))
            .collect()
    }

    fn frame_columns(&self, at: usize, width: usize) -> Columns {
        self.columns[at..at + width]
            .iter()
            .map(|c| (c.qualifier.clone(), c.name.clone()))
            .collect()
    }

    fn star(&self) -> Vec<usize> {
        self.items.iter().flat_map(|n| n.star.clone()).collect()
    }
}

/// What laying a FROM out reads: the catalog, the steps made before, the step's functions.
struct Laying<'a> {
    catalog: &'a Catalog,
    made: &'a HashMap<String, Made>,
    calls: &'a [Call],
    release: u8,
    columns: Vec<Col>,
}

impl Laying<'_> {
    fn factor(&mut self, t: &TableFactor) -> Result<Node> {
        let at = self.columns.len();
        let TableFactor::Table { name, alias, .. } = t else {
            let TableFactor::NestedJoin {
                table_with_joins, ..
            } = t
            else {
                return Err(Error::new(format!("{t} is not read in FROM")));
            };
            return self.item(table_with_joins);
        };
        let n = crate::catalog::object_name(name);
        let qualifier = |default: &str| {
            alias
                .as_ref()
                .map(|a| crate::catalog::ident(&a.name))
                .unwrap_or_else(|| default.rsplit('.').next().unwrap_or(default).to_string())
        };
        let renames = renames_of(alias)?;
        if let Some(i) = n.strip_prefix(CALLED) {
            let call = self.calls[i.parse::<usize>().unwrap()].clone();
            return self.call(call, alias, renames);
        }
        let (what, cols): (What, Vec<Col>) = if let Some(m) = self.made.get(&n) {
            let q = qualifier(&n);
            let cols = m
                .rel
                .names
                .iter()
                .zip(&m.types)
                .zip(&m.carried)
                .map(|((c, t), k)| Col {
                    qualifier: Some(q.clone()),
                    name: c.clone(),
                    ty: *t,
                    carried: k.clone(),
                })
                .collect();
            (
                What::Made {
                    name: n.clone(),
                    qualifier: q,
                },
                cols,
            )
        } else {
            let ti = table_of(self.catalog, &n)?;
            let table = &self.catalog.tables[ti];
            let q = qualifier(&table.name);
            let carried = Casings::of_table(table, &self.catalog.collations);
            let carried = carried.relation(&table.name).unwrap_or_default();
            let cols = stored(table)
                .map(|c| Col {
                    qualifier: Some(q.clone()),
                    name: table.columns[c].name.clone(),
                    ty: Some(table.columns[c].ty),
                    carried: carried
                        .iter()
                        .find(|(m, _)| *m == table.columns[c].name)
                        .map_or(Carried::None, |(_, k)| k.clone()),
                })
                .collect();
            (
                What::Table {
                    table: ti,
                    qualifier: q,
                },
                cols,
            )
        };
        let mut cols = cols;
        if renames.len() > cols.len() {
            return Err(Error::new(format!(
                "table \"{}\" has {} columns available but {} columns specified",
                alias.as_ref().map(|a| a.name.value.clone()).unwrap_or(n),
                cols.len(),
                renames.len()
            )));
        }
        for (c, r) in cols.iter_mut().zip(&renames) {
            c.name = r.clone();
        }
        let width = cols.len();
        self.columns.extend(cols);
        Ok(Node {
            what,
            at,
            width,
            star: (at..at + width).collect(),
        })
    }

    /// A function's rows: one column of its type, named by its one OUT parameter, else by the alias
    /// or the function; a record's fields each a column; and its ordinality after, where asked.
    fn call(
        &mut self,
        call: Call,
        alias: &Option<TableAlias>,
        renames: Vec<String>,
    ) -> Result<Node> {
        let at = self.columns.len();
        let Expr::Function(f) = &call.expr else {
            unreachable!("a call")
        };
        let name = pipeline::function_name(f);
        if !pipeline::is_set_returning(&name) {
            return Err(Error::new(format!(
                "{}: a function in FROM is read where it returns a set the snail makes",
                call.expr
            )));
        }
        let typed: Vec<TypedColumn> = self
            .columns
            .iter()
            .map(|c| (c.qualifier.clone(), c.name.clone(), c.ty))
            .collect();
        let mut typing = typed::Typing {
            columns: &typed,
            release: self.release,
            types: typed::Types::new(),
        };
        let ty = typing.type_of(&call.expr)?;
        let q = alias
            .as_ref()
            .map(|a| crate::catalog::ident(&a.name))
            .unwrap_or_else(|| name.clone());
        let fields = ty.and_then(record_fields);
        let mut cols: Vec<Col> = match fields {
            Some(fields) => fields
                .into_iter()
                .map(|(n, t)| Col {
                    qualifier: Some(q.clone()),
                    name: n,
                    ty: Some(t),
                    carried: carried_of(Some(t)),
                })
                .collect(),
            None => {
                let named = match name.as_str() {
                    "json_array_elements"
                    | "json_array_elements_text"
                    | "jsonb_array_elements"
                    | "jsonb_array_elements_text" => "value".to_string(),
                    _ => alias
                        .as_ref()
                        .map(|a| crate::catalog::ident(&a.name))
                        .unwrap_or_else(|| name.clone()),
                };
                vec![Col {
                    qualifier: Some(format!("{}{q}", pipeline::SCALAR)),
                    name: named,
                    ty,
                    carried: carried_of(ty),
                }]
            }
        };
        if call.ordinality {
            cols.push(Col {
                qualifier: Some(q.clone()),
                name: "ordinality".into(),
                ty: Some(ColumnType::Int8),
                carried: Carried::None,
            });
        }
        if renames.len() > cols.len() {
            return Err(Error::new(format!(
                "table \"{q}\" has {} columns available but {} columns specified",
                cols.len(),
                renames.len()
            )));
        }
        for (c, r) in cols.iter_mut().zip(&renames) {
            c.name = r.clone();
        }
        let width = cols.len();
        self.columns.extend(cols);
        Ok(Node {
            what: What::Call { call, qualifier: q },
            at,
            width,
            star: (at..at + width).collect(),
        })
    }

    /// A FROM item: its first relation, then each joined to it in turn.
    fn item(&mut self, item: &TableWithJoins) -> Result<Node> {
        let mut node = self.factor(&item.relation)?;
        for j in &item.joins {
            let right = self.factor(&j.relation)?;
            let (kind, constraint) = match &j.join_operator {
                JoinOperator::Join(c) | JoinOperator::Inner(c) => (Kind::Inner, c),
                JoinOperator::CrossJoin(c) => (Kind::Cross, c),
                JoinOperator::Left(c) | JoinOperator::LeftOuter(c) => (Kind::Left, c),
                JoinOperator::Right(c) | JoinOperator::RightOuter(c) => (Kind::Right, c),
                JoinOperator::FullOuter(c) => (Kind::Full, c),
                other => {
                    return Err(Error::new(format!(
                        "{} is not read",
                        format!("{other:?}").split('(').next().unwrap_or_default()
                    )))
                }
            };
            let (on, using) = match constraint {
                JoinConstraint::On(e) if kind != Kind::Cross => (Some(e.clone()), Vec::new()),
                JoinConstraint::None if kind == Kind::Cross => (None, Vec::new()),
                JoinConstraint::Using(names) => (
                    None,
                    names
                        .iter()
                        .map(crate::catalog::object_name)
                        .collect::<Vec<_>>(),
                ),
                JoinConstraint::Natural => {
                    let left = self.visible(&node);
                    let right = self.visible(&right);
                    (
                        None,
                        left.into_iter()
                            .filter(|n| right.contains(n))
                            .collect::<Vec<_>>(),
                    )
                }
                _ => return Err(Error::new("a JOIN is read with its ON, USING or NATURAL")),
            };
            // NATURAL with no column in common is a cross join.
            let kind = match (kind, &constraint, using.is_empty(), &on) {
                (Kind::Inner, JoinConstraint::Natural, true, None) => Kind::Cross,
                (k, _, _, _) => k,
            };
            node = self.join(node, right, kind, on, &using)?;
        }
        Ok(node)
    }

    /// Whether a FULL JOIN's ON is one PostgreSQL meets the rows by: true, or holding an = between
    /// an expression of the left side's columns alone and one of the right side's.
    fn hashable(&self, left: &Node, right: &Node, on: &Expr) -> bool {
        if matches!(on, Expr::Value(v) if v.value == sqlparser::ast::Value::Boolean(true)) {
            return true;
        }
        let side = |e: &Expr| -> Option<bool> {
            let mut read = Vec::new();
            pipeline::columns_read(e, &mut read);
            let mut sides = read.iter().map(|(q, n)| {
                match pipeline::naming(
                    self.columns[left.at..right.at + right.width]
                        .iter()
                        .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
                    q.as_deref(),
                    n,
                )
                .as_slice()
                {
                    [i] => Some(*i < left.width),
                    _ => None,
                }
            });
            let first = sides.next()??;
            sides.all(|s| s == Some(first)).then_some(first)
        };
        let mut cs = Vec::new();
        conjuncts(on, &mut cs);
        cs.into_iter().any(|c| match c {
            Expr::BinaryOp {
                left: a,
                op: BinaryOperator::Eq,
                right: b,
            } => matches!((side(a), side(b)), (Some(x), Some(y)) if x != y),
            _ => false,
        })
    }

    /// The names a bare reference finds in a node's columns, each once, in order.
    fn visible(&self, n: &Node) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for c in &self.columns[n.at..n.at + n.width] {
            if !c
                .qualifier
                .as_deref()
                .is_some_and(|q| q.starts_with(MERGED))
                && !out.contains(&c.name)
            {
                out.push(c.name.clone());
            }
        }
        out
    }

    /// The one column of a node a bare name finds, for a USING: refused as PostgreSQL refuses where
    /// it finds none or two.
    fn using_column(&self, n: &Node, name: &str, side: &str) -> Result<usize> {
        let found = pipeline::naming(
            self.columns[n.at..n.at + n.width]
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            None,
            name,
        );
        match found.as_slice() {
            [i] => Ok(n.at + i),
            [] => Err(Error::new(format!(
                "column \"{name}\" specified in USING clause does not exist in {side} table"
            ))),
            _ => Err(Error::new(format!(
                "common column name \"{name}\" appears more than once in {side} table"
            ))),
        }
    }

    fn join(
        &mut self,
        left: Node,
        right: Node,
        kind: Kind,
        on: Option<Expr>,
        using: &[String],
    ) -> Result<Node> {
        let at = left.at;
        if let (Kind::Full, Some(e)) = (kind, &on) {
            if !self.hashable(&left, &right, e) {
                return Err(Error::new(
                    "FULL JOIN is only supported with merge-joinable or hash-joinable join \
                     conditions",
                ));
            }
        }
        let mut merged = Vec::new();
        let mut hidden = Vec::new();
        for (k, name) in using.iter().enumerate() {
            if using[..k].contains(name) {
                return Err(Error::new(format!(
                    "column name \"{name}\" appears more than once in USING clause"
                )));
            }
            let l = self.using_column(&left, name, "left")?;
            let r = self.using_column(&right, name, "right")?;
            let ty = match (self.columns[l].ty, self.columns[r].ty) {
                (Some(a), Some(b)) => Some(typed::common_type(&[Some(a), Some(b)], "JOIN/USING")?),
                (a, b) => a.or(b),
            };
            let carried = match (&self.columns[l].carried, &self.columns[r].carried) {
                (a, b) if a == b => a.clone(),
                (Carried::None, b) => b.clone(),
                (a, Carried::None) => a.clone(),
                _ => Carried::Unsettled,
            };
            merged.push((l, r, ty, carried, name.clone()));
            hidden.extend([l, r]);
        }
        for &i in &hidden {
            let q = self.columns[i].qualifier.clone().unwrap_or_default();
            self.columns[i].qualifier = Some(format!("{MERGED}{}", q.trim_start_matches(MERGED)));
        }
        let mut star: Vec<usize> = Vec::new();
        let mut made = Vec::new();
        for (l, r, ty, carried, name) in merged {
            let place = self.columns.len();
            self.columns.push(Col {
                qualifier: None,
                name,
                ty,
                carried,
            });
            star.push(place);
            made.push(Merged {
                at: place,
                left: l,
                right: r,
                ty,
            });
        }
        star.extend(left.star.iter().filter(|i| !hidden.contains(i)));
        star.extend(right.star.iter().filter(|i| !hidden.contains(i)));
        let width = self.columns.len() - at;
        Ok(Node {
            what: What::Join {
                left: Box::new(left),
                right: Box::new(right),
                kind,
                on,
                merged: made,
            },
            at,
            width,
            star,
        })
    }
}

/// The fields of a record type, each with its type.
fn record_fields(ty: ColumnType) -> Option<Vec<(String, ColumnType)>> {
    let ColumnType::Composed(r) = ty else {
        return None;
    };
    match r.def() {
        crate::composed::Composed::Record { fields, .. } => {
            Some(fields.iter().map(|f| (f.name.clone(), f.ty)).collect())
        }
        _ => None,
    }
}

/// The collation a value of a type carries where nothing names one: the database's, for a string.
fn carried_of(ty: Option<ColumnType>) -> Carried {
    match ty {
        Some(ColumnType::Name) => Carried::Named("C".into()),
        Some(t) if super::collate::collatable(t) => Carried::Named(super::collate::DEFAULT.into()),
        _ => Carried::None,
    }
}

/// The columns a table stores: a step computes each virtual one from them.
fn stored(table: &Table) -> impl Iterator<Item = usize> + '_ {
    (0..table.columns.len()).filter(|&c| !table.columns[c].is_virtual())
}

/// A table of the catalog by its name, or the last part of it.
fn table_of(catalog: &Catalog, n: &str) -> Result<usize> {
    catalog
        .tables
        .iter()
        .position(|t| t.name == n)
        .or_else(|| {
            catalog
                .tables
                .iter()
                .position(|t| t.name.rsplit('.').next() == Some(n))
        })
        .ok_or_else(|| Error::new(format!("relation \"{n}\" does not exist")))
}

/// A name quoted as SQL writes it.
fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A table's name as SQL writes it, each part quoted.
fn quote_name(s: &str) -> String {
    s.split('.').map(quote).collect::<Vec<_>>().join(".")
}

/// The conjuncts of a condition.
fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
    match e {
        Expr::BinaryOp {
            left,
            op: BinaryOperator::And,
            right,
        } => {
            conjuncts(left, out);
            conjuncts(right, out);
        }
        Expr::Nested(x) => conjuncts(x, out),
        e => out.push(e),
    }
}

/// The words of a text as sqlparser's tokenizer reads them: a bare word folded, a quoted one as it
/// is.
fn words(text: &str) -> Vec<String> {
    sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, text)
        .tokenize()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| match t {
            sqlparser::tokenizer::Token::Word(w) => Some(match w.quote_style {
                Some(_) => w.value,
                None => w.value.to_lowercase(),
            }),
            sqlparser::tokenizer::Token::Placeholder(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// A step's FROM read into its rows, as the round reads the tables in it.
struct Reading<'a> {
    reads: &'a dyn Reads,
    from: &'a From,
    /// The words the step writes: a column none names is not read.
    words: &'a [String],
    /// The conjuncts of its WHERE, which a table read alone or a join read as C1's takes where it
    /// reads them.
    wheres: Vec<&'a Expr>,
    params: &'a [String],
    release: u8,
    casings: &'a Casings,
    types: &'a typed::Types,
    now: i64,
    made: &'a HashMap<String, Made>,
    accrued: &'a mut Accrued,
    /// What a node's read made, where it is read once.
    cached: HashMap<*const Node, Vec<Vec<Value>>>,
    /// Whether the rows a node makes keep their table's order, at a column of theirs.
    order: Option<usize>,
}

/// What the rounds a statement reads have read, in all and on each snail.
#[derive(Default)]
struct Accrued {
    round: Round,
    parts: Vec<Round>,
}

impl Accrued {
    fn add(&mut self, r: &Round) {
        let order = r.order.clone();
        self.round.add_table(r, &order);
        self.round.rows_walked += r.rows_walked;
        self.round.rows_taken += r.rows_taken;
        if self.parts.len() < r.snails.len() {
            self.parts.resize(r.snails.len(), Round::default());
        }
        for (p, s) in self.parts.iter_mut().zip(&r.snails) {
            let order = s.order.clone();
            p.add_table(s, &order);
            p.rows_walked += s.rows_walked;
            p.rows_taken += s.rows_taken;
        }
    }
}

impl Reading<'_> {
    /// Whether a column at a place is one the step names, by its name or by its place.
    fn needed(&self, place: usize) -> bool {
        let c = &self.from.columns[place];
        self.words.contains(&c.name) || self.words.contains(&format!("{MERGED}{place}"))
    }

    /// The conjuncts of WHERE a table read alone may take: those that name no other relation's
    /// column, nor a column of its own another relation names alike.
    fn own_wheres(&self, node: &Node) -> Vec<&Expr> {
        let range = node.at..node.at + node.width;
        let theirs: Vec<&str> = self
            .from
            .columns
            .iter()
            .enumerate()
            .filter(|(i, _)| !range.contains(i))
            .flat_map(|(_, c)| {
                std::iter::once(c.name.as_str())
                    .chain(c.qualifier.as_deref().map(pipeline::unmarked))
            })
            .collect();
        self.wheres
            .iter()
            .copied()
            .filter(|w| {
                let ws = words(&w.to_string());
                !ws.iter()
                    .any(|x| theirs.contains(&x.as_str()) || x.starts_with('$'))
            })
            .collect()
    }

    /// A node's rows, each its `width` values, made with the row before it where it is lateral:
    /// `before` the values of the FROM's columns from `start` to the node.
    fn rows(
        &mut self,
        node: &Node,
        before: Option<(usize, &[Value])>,
        preserved: bool,
    ) -> Result<Vec<Vec<Value>>> {
        if let Some(r) = self.cached.get(&(node as *const Node)) {
            return Ok(r.clone());
        }
        let rows = match &node.what {
            What::Table { table, qualifier } => {
                let r = self.alone(node, *table, qualifier, preserved)?;
                self.cached.insert(node, r.clone());
                r
            }
            What::Made { name, .. } => {
                let r = self.made[name].rel.rows.clone();
                self.cached.insert(node, r.clone());
                r
            }
            What::Call { call, .. } => self.called(node, call, before)?,
            What::Join {
                left,
                right,
                kind,
                on,
                merged,
            } => {
                // A join's rows keep no table's order.
                self.order = None;
                if let Some(r) = self.together(node, preserved)? {
                    self.cached.insert(node, r.clone());
                    return Ok(r);
                }
                let keep = |side: bool| match kind {
                    Kind::Inner | Kind::Cross => preserved,
                    Kind::Left => preserved && side,
                    Kind::Right => preserved && !side,
                    Kind::Full => false,
                };
                let lrows = self.rows(left, before, keep(true))?;
                let rlateral = right.lateral();
                let rrows = match rlateral {
                    true => None,
                    false => Some(self.rows(right, before, keep(false))?),
                };
                let rows = self.met(
                    node,
                    left,
                    right,
                    *kind,
                    on.as_ref(),
                    merged,
                    lrows,
                    rrows,
                    before,
                )?;
                self.order = None;
                rows
            }
        };
        Ok(rows)
    }

    /// A table read alone, along the order whose pages are fewest, by the conditions of WHERE on
    /// it alone the question path reads, where nothing may leave it NULL: its rows, each its columns
    /// in place, NULL where the step names none.
    fn alone(
        &mut self,
        node: &Node,
        table: usize,
        qualifier: &str,
        preserved: bool,
    ) -> Result<Vec<Vec<Value>>> {
        let catalog = self.reads.catalog();
        let t = &catalog.tables[table];
        let mut wanted: Vec<usize> = (node.at..node.at + node.width)
            .filter(|&p| self.needed(p))
            .collect();
        if wanted.is_empty() {
            wanted.push(node.at);
        }
        let names: Vec<String> = stored(t).map(|c| t.columns[c].name.clone()).collect();
        let select = wanted
            .iter()
            .map(|&p| {
                format!(
                    "{}.{} AS {}",
                    quote(qualifier),
                    quote(&names[p - node.at]),
                    quote(&format!("c{p}"))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let from = format!("{} {}", quote_name(&t.name), quote(qualifier));
        let conds: Vec<&Expr> = match preserved {
            true => self.own_wheres(node),
            false => Vec::new(),
        };
        let q = self.parsed(&select, &from, &conds)?;
        let (batch, round) = self.reads.read(&q, None)?;
        self.accrued.add(&round);
        let rel = pipeline::relation_of(&batch)?;
        let mut rows: Vec<Vec<Value>> = rel
            .rows
            .into_iter()
            .map(|r| {
                let mut row = vec![Value::Null; node.width];
                for (v, &p) in r.into_iter().zip(&wanted) {
                    row[p - node.at] = v;
                }
                row
            })
            .collect();
        // Its rows in the table's order, where that order is one unique column it holds: a window
        // reads along it.
        if let Some(c) = along(t) {
            if let Some(p) = stored(t).position(|x| x == c) {
                let mut rel = Relation {
                    names: Vec::new(),
                    rows,
                    order: None,
                };
                if wanted.contains(&(node.at + p)) {
                    pipeline::in_order(&mut rel, p)?;
                    self.order = Some(node.at + p);
                }
                rows = rel.rows;
            }
        }
        Ok(rows)
    }

    /// The question the question path reads: the select list, the FROM and as many of the
    /// conditions as it reads together; with none, where it reads them not.
    fn parsed(&self, select: &str, from: &str, conds: &[&Expr]) -> Result<Question> {
        let catalog = self.reads.catalog();
        let text = |cs: &[&Expr]| match cs {
            [] => format!("SELECT {select} FROM {from}"),
            cs => format!(
                "SELECT {select} FROM {from} WHERE {}",
                cs.iter()
                    .map(|c| format!("({c})"))
                    .collect::<Vec<_>>()
                    .join(" AND ")
            ),
        };
        let read: Vec<&Expr> = conds
            .iter()
            .copied()
            .filter(|c| question::parse(catalog, &text(&[c])).is_ok())
            .collect();
        match question::parse(catalog, &text(&read)) {
            Ok(q) => Ok(q),
            Err(_) => question::parse(catalog, &text(&[])),
        }
    }

    /// A join whose every relation is a table, read as C1's round meets them where the question
    /// path reads the join; none where it does not.
    fn together(&mut self, node: &Node, preserved: bool) -> Result<Option<Vec<Vec<Value>>>> {
        let mut leaves = Vec::new();
        node.leaves(&mut leaves);
        if leaves.iter().any(|l| !matches!(l.what, What::Table { .. })) {
            return Ok(None);
        }
        let Some(from) = self.written(node, true)? else {
            return Ok(None);
        };
        let catalog = self.reads.catalog();
        let mut wanted: Vec<(usize, String, String)> = Vec::new();
        for l in &leaves {
            let What::Table { table, qualifier } = &l.what else {
                unreachable!()
            };
            let t = &catalog.tables[*table];
            let names: Vec<String> = stored(t).map(|c| t.columns[c].name.clone()).collect();
            for p in l.at..l.at + l.width {
                if self.needed(p) || self.merges(node, p) {
                    wanted.push((p, qualifier.clone(), names[p - l.at].clone()));
                }
            }
        }
        if wanted.is_empty() {
            let l = leaves[0];
            let What::Table { table, qualifier } = &l.what else {
                unreachable!()
            };
            let t = &catalog.tables[*table];
            let first = stored(t).next().unwrap();
            wanted.push((l.at, qualifier.clone(), t.columns[first].name.clone()));
        }
        let select = wanted
            .iter()
            .map(|(p, q, n)| format!("{}.{} AS {}", quote(q), quote(n), quote(&format!("c{p}"))))
            .collect::<Vec<_>>()
            .join(", ");
        let conds: Vec<&Expr> = match preserved {
            true => self.wheres.clone(),
            false => Vec::new(),
        };
        let q = match self.parsed(&select, &from, &conds) {
            Ok(q) => q,
            Err(_) => return Ok(None),
        };
        let (batch, round) = self.reads.read(&q, None)?;
        self.accrued.add(&round);
        let rel = pipeline::relation_of(&batch)?;
        let mut rows = Vec::with_capacity(rel.rows.len());
        for r in rel.rows {
            let mut row = vec![Value::Null; node.width];
            for (v, (p, _, _)) in r.into_iter().zip(&wanted) {
                row[p - node.at] = v;
            }
            Self::merge(node, node.at, &mut row)?;
            rows.push(row);
        }
        Ok(Some(rows))
    }

    /// Whether a column at a place is one a USING of the node merges, read so its merged column can
    /// be made.
    fn merges(&self, node: &Node, place: usize) -> bool {
        match &node.what {
            What::Join {
                left,
                right,
                merged,
                ..
            } => {
                merged
                    .iter()
                    .any(|m| (m.left == place || m.right == place) && self.needed(m.at))
                    || self.merges(left, place)
                    || self.merges(right, place)
            }
            _ => false,
        }
    }

    /// A join of tables as the question path reads it, each USING written as its ON: none where it
    /// writes no such join. `first` where the join is the first of its FROM item.
    fn written(&self, node: &Node, _first: bool) -> Result<Option<String>> {
        let catalog = self.reads.catalog();
        Ok(Some(match &node.what {
            What::Table { table, qualifier } => format!(
                "{} {}",
                quote_name(&catalog.tables[*table].name),
                quote(qualifier)
            ),
            What::Join {
                left,
                right,
                kind,
                on,
                merged,
            } => {
                // The question path reads a join of a join and a table, left-deep.
                if matches!(right.what, What::Join { .. }) {
                    return Ok(None);
                }
                let Some(l) = self.written(left, false)? else {
                    return Ok(None);
                };
                let Some(r) = self.written(right, false)? else {
                    return Ok(None);
                };
                let mut conds: Vec<String> = Vec::new();
                for m in merged {
                    if !matches!(kind, Kind::Inner | Kind::Left) {
                        return Ok(None);
                    }
                    let (Some(a), Some(b)) =
                        (self.source(left, m.left), self.source(right, m.right))
                    else {
                        return Ok(None);
                    };
                    conds.push(format!("{a} = {b}"));
                }
                if let Some(on) = on {
                    // A bare name a USING merged reads the merged column, which the question path
                    // does not name.
                    let ws = words(&on.to_string());
                    if self.from.columns[node.at..node.at + node.width]
                        .iter()
                        .any(|c| c.qualifier.is_none() && ws.contains(&c.name))
                    {
                        return Ok(None);
                    }
                    conds.push(on.to_string());
                }
                let joining = match kind {
                    Kind::Inner => "JOIN",
                    Kind::Left => "LEFT JOIN",
                    Kind::Right => "RIGHT JOIN",
                    Kind::Full => "FULL JOIN",
                    Kind::Cross => "CROSS JOIN",
                };
                match (kind, conds.is_empty()) {
                    (Kind::Cross, _) => format!("{l} CROSS JOIN {r}"),
                    (_, true) => return Ok(None),
                    _ => format!("{l} {joining} {r} ON {}", conds.join(" AND ")),
                }
            }
            _ => return Ok(None),
        }))
    }

    /// The column of a table a merged column at a place holds the values of, as SQL names it: the
    /// table's own, or what a USING below it merged from the left.
    fn source(&self, node: &Node, place: usize) -> Option<String> {
        let c = &self.from.columns[place];
        if let Some(q) = &c.qualifier {
            return Some(format!(
                "{}.{}",
                quote(pipeline::unmarked(q)),
                quote(&c.name)
            ));
        }
        match &node.what {
            What::Join {
                left,
                right,
                kind: Kind::Inner | Kind::Left,
                merged,
                ..
            } => match merged.iter().find(|m| m.at == place) {
                Some(m) => self.source(left, m.left),
                None if (left.at..left.at + left.width).contains(&place) => {
                    self.source(left, place)
                }
                None => self.source(right, place),
            },
            _ => None,
        }
    }

    /// The columns a node's USINGs merge, made in a row of its values from `at`: the left side's
    /// value, the right side's for a RIGHT JOIN, the first not NULL for a FULL JOIN; each in the
    /// type the two meet in.
    fn merge(node: &Node, at: usize, row: &mut [Value]) -> Result<()> {
        let What::Join {
            left,
            right,
            kind,
            merged,
            ..
        } = &node.what
        else {
            return Ok(());
        };
        Self::merge(left, at, row)?;
        Self::merge(right, at, row)?;
        for m in merged {
            let (l, r) = (&row[m.left - at], &row[m.right - at]);
            let v = match kind {
                Kind::Right => r.clone(),
                Kind::Full if l.is_null() => r.clone(),
                _ => l.clone(),
            };
            row[m.at - at] = match m.ty {
                Some(ty) if !v.is_null() => coerce(v, ty, false)?,
                _ => v,
            };
        }
        Ok(())
    }

    /// A function's rows for the row before it, or for none where nothing comes before it.
    fn called(
        &mut self,
        node: &Node,
        call: &Call,
        before: Option<(usize, &[Value])>,
    ) -> Result<Vec<Vec<Value>>> {
        let (start, row) = before.unwrap_or((node.at, &[]));
        let frame = pipeline::Frame {
            columns: self.from.frame_columns(start, row.len()),
            params: self.params,
            release: self.release,
            casings: self.casings,
            types: self.types,
            now: self.now,
        };
        let values = pipeline::call_rows(&call.expr, row, &frame)?;
        let fields = node.width - call.ordinality as usize;
        values
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let mut out = match (fields, &v) {
                    (1, _) => vec![v],
                    (_, Value::Null) => vec![Value::Null; fields],
                    (_, v) => pipeline::record_values(v, fields)?,
                };
                if call.ordinality {
                    out.push(Value::Int(i as i64 + 1));
                }
                Ok(out)
            })
            .collect()
    }

    /// Two relations met: each pair of their rows the ON holds of, or every pair; a row of the side
    /// an outer join keeps that meets none, beside NULLs; then the columns the USING merges. The
    /// right side's rows are made for each row of the left where it is lateral.
    #[allow(clippy::too_many_arguments)]
    fn met(
        &mut self,
        node: &Node,
        left: &Node,
        right: &Node,
        kind: Kind,
        on: Option<&Expr>,
        merged: &[Merged],
        lrows: Vec<Vec<Value>>,
        rrows: Option<Vec<Vec<Value>>>,
        before: Option<(usize, &[Value])>,
    ) -> Result<Vec<Vec<Value>>> {
        let frame_columns = self.from.frame_columns(node.at, left.width + right.width);
        let frame = pipeline::Frame {
            columns: frame_columns,
            params: self.params,
            release: self.release,
            casings: self.casings,
            types: self.types,
            now: self.now,
        };
        let none = pipeline::computed_none();
        // The pairs of places a USING meets, compared by =.
        let usings: Vec<(usize, usize)> = merged
            .iter()
            .map(|m| (m.left - node.at, m.right - node.at))
            .collect();
        let mut out = Vec::new();
        let width = left.width + right.width;
        let holds = |pair: &[Value]| -> Result<bool> {
            for &(a, b) in &usings {
                if pipeline::equal(&pair[a], &pair[b])? != Some(true) {
                    return Ok(false);
                }
            }
            match on {
                Some(e) => {
                    Ok(pipeline::truth(&pipeline::eval(e, pair, &frame, &none)?)? == Some(true))
                }
                None => Ok(true),
            }
        };
        let finish = |mut pair: Vec<Value>| -> Result<Vec<Value>> {
            pair.resize(node.width, Value::Null);
            Self::merge(node, node.at, &mut pair)?;
            Ok(pair)
        };
        match rrows {
            Some(rrows) => {
                let mut right_met = vec![false; rrows.len()];
                let index = self.hashed(node, left, on, &usings, &rrows, &frame)?;
                for l in &lrows {
                    let mut any = false;
                    let candidates: Box<dyn Iterator<Item = usize>> = match &index {
                        Some((probe, map)) => {
                            let k = probe(l)?;
                            match k.and_then(|k| map.get(&k)) {
                                Some(v) => Box::new(v.clone().into_iter()),
                                None => Box::new(std::iter::empty()),
                            }
                        }
                        None => Box::new(0..rrows.len()),
                    };
                    for i in candidates {
                        let mut pair = Vec::with_capacity(width);
                        pair.extend(l.iter().cloned());
                        pair.extend(rrows[i].iter().cloned());
                        if holds(&pair)? {
                            any = true;
                            right_met[i] = true;
                            out.push(finish(pair)?);
                        }
                    }
                    if !any && matches!(kind, Kind::Left | Kind::Full) {
                        let mut pair = l.clone();
                        pair.resize(width, Value::Null);
                        out.push(finish(pair)?);
                    }
                }
                if matches!(kind, Kind::Right | Kind::Full) {
                    for (r, met) in rrows.iter().zip(&right_met) {
                        if !met {
                            let mut pair = vec![Value::Null; left.width];
                            pair.extend(r.iter().cloned());
                            out.push(finish(pair)?);
                        }
                    }
                }
            }
            None => {
                if matches!(kind, Kind::Right | Kind::Full) {
                    return Err(Error::new(
                        "invalid reference to FROM-clause entry: a RIGHT or FULL JOIN to a LATERAL \
                         function",
                    ));
                }
                for l in &lrows {
                    // The rows before it, and the left side's.
                    let mut ctx: Vec<Value> = before.map(|(_, r)| r.to_vec()).unwrap_or_default();
                    let start = before.map_or(left.at, |(s, _)| s);
                    ctx.extend(l.iter().cloned());
                    let rs = self.rows(right, Some((start, &ctx)), false)?;
                    let mut any = false;
                    for r in rs {
                        let mut pair = Vec::with_capacity(width);
                        pair.extend(l.iter().cloned());
                        pair.extend(r);
                        if holds(&pair)? {
                            any = true;
                            out.push(finish(pair)?);
                        }
                    }
                    if !any && kind == Kind::Left {
                        let mut pair = l.clone();
                        pair.resize(width, Value::Null);
                        out.push(finish(pair)?);
                    }
                }
            }
        }
        Ok(out)
    }

    /// The right side's rows by the key each conjunct of the ON comparing a column of each side by
    /// = gives, and how a left row's key is made; none where no conjunct is one, or one of them
    /// compares under a library's collation or across types whose keys differ.
    #[allow(clippy::type_complexity)]
    fn hashed<'f>(
        &self,
        node: &Node,
        left: &Node,
        on: Option<&'f Expr>,
        usings: &[(usize, usize)],
        rrows: &[Vec<Value>],
        frame: &'f pipeline::Frame<'f>,
    ) -> Result<
        Option<(
            Box<dyn Fn(&[Value]) -> Result<Option<Vec<pipeline::Key>>> + 'f>,
            HashMap<Vec<pipeline::Key>, Vec<usize>>,
        )>,
    > {
        let lw = left.width;
        let side = |e: &Expr| -> Option<bool> {
            let (q, n) = pipeline::column_of(e)?;
            let found = pipeline::naming(
                frame
                    .columns
                    .iter()
                    .map(|(q, n)| (q.as_deref(), n.as_str())),
                q.as_deref(),
                &n,
            );
            match found.as_slice() {
                [i] => Some(*i < lw),
                _ => None,
            }
        };
        // Each pair: the left side's expression or place, the right side's.
        let mut pairs: Vec<(Keyed<'f>, Keyed<'f>)> = usings
            .iter()
            .map(|&(a, b)| (Keyed::At(a), Keyed::At(b)))
            .collect();
        let mut cs = Vec::new();
        if let Some(e) = on {
            conjuncts(e, &mut cs);
        }
        for c in cs {
            let Expr::BinaryOp {
                left: a,
                op: BinaryOperator::Eq,
                right: b,
            } = c
            else {
                continue;
            };
            let (ta, tb) = (
                self.types.get(&(&**a as *const Expr)),
                self.types.get(&(&**b as *const Expr)),
            );
            let same = match (ta, tb) {
                (Some(x), Some(y)) => x == y || (x.is_integer() && y.is_integer()),
                _ => false,
            };
            if !same || self.casings.compared_under_library(c) {
                continue;
            }
            match (side(a), side(b)) {
                (Some(true), Some(false)) => pairs.push((Keyed::Expr(a), Keyed::Expr(b))),
                (Some(false), Some(true)) => pairs.push((Keyed::Expr(b), Keyed::Expr(a))),
                _ => {}
            }
        }
        if pairs.is_empty() {
            return Ok(None);
        }
        let _ = node;
        let none = pipeline::computed_none();
        let rw = frame.columns.len() - lw;
        let key_of = move |k: &Keyed, pair: &[Value]| -> Result<Option<pipeline::Key>> {
            let v = match k {
                Keyed::At(i) => pair[*i].clone(),
                Keyed::Expr(e) => pipeline::eval(e, pair, frame, &none)?,
            };
            Ok((!v.is_null()).then(|| pipeline::key(&v)))
        };
        let mut map: HashMap<Vec<pipeline::Key>, Vec<usize>> = HashMap::new();
        for (i, r) in rrows.iter().enumerate() {
            let mut pair = vec![Value::Null; lw];
            pair.extend(r.iter().cloned());
            let mut k = Vec::with_capacity(pairs.len());
            let mut null = false;
            for (_, b) in &pairs {
                match key_of(b, &pair)? {
                    Some(x) => k.push(x),
                    None => null = true,
                }
            }
            if !null {
                map.entry(k).or_default().push(i);
            }
        }
        let lefts: Vec<Keyed<'f>> = pairs.into_iter().map(|(a, _)| a).collect();
        let probe = move |l: &[Value]| -> Result<Option<Vec<pipeline::Key>>> {
            let mut pair = l.to_vec();
            pair.resize(lw + rw, Value::Null);
            let mut k = Vec::with_capacity(lefts.len());
            for a in &lefts {
                match key_of(a, &pair)? {
                    Some(x) => k.push(x),
                    None => return Ok(None),
                }
            }
            Ok(Some(k))
        };
        Ok(Some((Box::new(probe), map)))
    }
}

/// One side of an equality a hash meets rows by: a column at its place, or an expression.
#[derive(Clone, Copy)]
enum Keyed<'e> {
    At(usize),
    Expr(&'e Expr),
}

/// A value in a type, as an implicit cast takes it.
fn coerce(v: Value, ty: ColumnType, quoted: bool) -> Result<Value> {
    Ok(pipeline::part_value(ty, &pipeline::part_of(ty, v, quoted)?))
}

/// The column holding a table's order where it carries a window: its one level, unique, a column
/// NOT NULL or holding at most one NULL, last.
fn along(table: &Table) -> Option<usize> {
    let order = table.orders.iter().find(|o| o.table_order)?;
    match (order.unique, order.levels.as_slice()) {
        (true, [crate::catalog::Level::Column(c)])
            if table.columns[*c].not_null || (!order.nulls_first[0] && !order.nulls_distinct) =>
        {
            Some(*c)
        }
        _ => None,
    }
}

/// A star in a select list read as the columns it names, each by its place in the FROM's row.
fn starred(projection: &mut Vec<SelectItem>, from: &From) -> Result<()> {
    let mut out = Vec::with_capacity(projection.len());
    let by_place = |places: Vec<usize>| -> Vec<SelectItem> {
        places
            .into_iter()
            .map(|p| SelectItem::ExprWithAlias {
                expr: Expr::Identifier(Ident::with_quote('"', format!("{MERGED}{p}"))),
                alias: Ident::with_quote('"', from.columns[p].name.clone()),
            })
            .collect()
    };
    for item in projection.drain(..) {
        match item {
            SelectItem::Wildcard(o) => {
                if o.to_string() != "" {
                    return Err(Error::new(format!("* {o} is not read")));
                }
                if from.items.is_empty() {
                    return Err(Error::new("SELECT * with no tables specified is not valid"));
                }
                out.extend(by_place(from.star()));
            }
            SelectItem::QualifiedWildcard(kind, o) => {
                if o.to_string() != "" {
                    return Err(Error::new(format!("{kind}.* {o} is not read")));
                }
                let q = match &kind {
                    sqlparser::ast::SelectItemQualifiedWildcardKind::ObjectName(n) => {
                        crate::catalog::object_name(n)
                    }
                    other => return Err(Error::new(format!("{other}.* is not read"))),
                };
                let mut leaves = Vec::new();
                for n in &from.items {
                    n.leaves(&mut leaves);
                }
                let found = leaves
                    .iter()
                    .find(|l| l.leaf_named() == Some(q.as_str()))
                    .ok_or_else(|| {
                        Error::new(format!("missing FROM-clause entry for table \"{q}\""))
                    })?;
                out.extend(by_place((found.at..found.at + found.width).collect()));
            }
            other => out.push(other),
        }
    }
    *projection = out;
    Ok(())
}

/// A VALUES list made: each column typed as select_common_type types its values, each value in it.
fn values_step(step: &Step, params: &[String], release: u8, casings: &Casings) -> Result<Made> {
    let SetExpr::Values(v) = step.query.body.as_ref() else {
        unreachable!("a VALUES step")
    };
    let rows: Vec<&Vec<Expr>> = v.rows.iter().map(|r| &r.content).collect();
    let n = rows.first().map_or(0, |r| r.len());
    if rows.iter().any(|r| r.len() != n) {
        return Err(Error::new("VALUES lists must all be the same length"));
    }
    let columns: Vec<TypedColumn> = Vec::new();
    let mut typing = typed::Typing {
        columns: &columns,
        release,
        types: typed::Types::new(),
    };
    let mut types = Vec::with_capacity(n);
    for c in 0..n {
        let mut ts = Vec::with_capacity(rows.len());
        for r in &rows {
            ts.push(typing.type_of(&r[c])?);
        }
        types.push(typed::common_type(&ts, "VALUES")?);
    }
    let frame = pipeline::Frame {
        columns: Vec::new(),
        params,
        release,
        casings,
        types: &typing.types,
        now: crate::scalars::transaction_clock(),
    };
    let none = pipeline::computed_none();
    let mut made = Vec::with_capacity(rows.len());
    for r in &rows {
        let mut row = Vec::with_capacity(n);
        for (e, &ty) in r.iter().zip(&types) {
            let x = pipeline::eval(e, &[], &frame, &none)?;
            row.push(match x.is_null() {
                true => Value::Null,
                false => coerce(x, ty, pipeline::quoted(e))?,
            });
        }
        made.push(row);
    }
    let names = (1..=n).map(|i| format!("column{i}")).collect();
    Ok(Made {
        rel: Relation {
            names,
            rows: made,
            order: None,
        },
        types: types.iter().map(|&t| Some(t)).collect(),
        carried: types.iter().map(|&t| carried_of(Some(t))).collect(),
    })
}

/// A question over several relations, answered: each step made in turn, its FROM met as one
/// relation, then read as the pipeline reads a step. One round, of every table read.
pub(super) fn answer(
    reads: &dyn Reads,
    q: &Query,
    params: &[String],
) -> Result<(RecordBatch, Round)> {
    let catalog = reads.catalog();
    let mut steps = steps(catalog, q)?;
    let release = catalog.release;
    let now = crate::scalars::transaction_clock();
    let tables: Vec<&Table> = catalog.tables.iter().collect();
    let mut casings = Casings::of_tables(&tables, &catalog.collations);
    let mut made: HashMap<String, Made> = HashMap::new();
    let mut accrued = Accrued {
        round: Round::default(),
        parts: vec![
            Round::default();
            if reads.snails() > 1 {
                reads.snails()
            } else {
                0
            }
        ],
    };
    let mut held = 0usize;
    for step in &mut steps {
        let m = match step.query.body.as_ref() {
            SetExpr::Values(_) => values_step(step, params, release, &casings)?,
            _ => select_step(
                reads,
                step,
                &made,
                &mut casings,
                params,
                release,
                now,
                &mut accrued,
            )?,
        };
        let mut m = m;
        if step.renames.len() > m.rel.names.len() {
            return Err(Error::new(format!(
                "WITH query \"{}\" has {} columns available but {} columns specified",
                step.name.trim_start_matches(MERGED),
                m.rel.names.len(),
                step.renames.len()
            )));
        }
        for (n, r) in m.rel.names.iter_mut().zip(&step.renames) {
            *n = r.clone();
        }
        casings.renamed(&step.name, &step.renames);
        held += m.rel.rows.len();
        made.insert(step.name.clone(), m);
    }
    let last = made
        .remove("")
        .ok_or_else(|| Error::new("the question returned nothing"))?;
    let batch = pipeline::batch_of(&last.rel)?;
    let mut round = accrued.round;
    round.rows = batch.num_rows() as u64;
    round.rows_held = held as u64;
    if reads.snails() > 1 {
        round.snails = accrued.parts;
    }
    Ok((batch, round))
}

/// One SELECT of the statement, its FROM met as one relation, then read as the pipeline reads it.
#[allow(clippy::too_many_arguments)]
fn select_step(
    reads: &dyn Reads,
    step: &mut Step,
    made: &HashMap<String, Made>,
    casings: &mut Casings,
    params: &[String],
    release: u8,
    now: i64,
    accrued: &mut Accrued,
) -> Result<Made> {
    let catalog = reads.catalog();
    // The FROM laid out, and its stars read as its columns.
    let from = {
        let SetExpr::Select(s) = step.query.body.as_ref() else {
            return Err(Error::new("a step is a SELECT"));
        };
        let mut laying = Laying {
            catalog,
            made,
            calls: &step.calls,
            release,
            columns: Vec::new(),
        };
        let mut items = Vec::with_capacity(s.from.len());
        for item in &s.from {
            items.push(laying.item(item)?);
        }
        From {
            columns: laying.columns,
            items,
        }
    };
    {
        let mut leaves = Vec::new();
        for n in &from.items {
            n.leaves(&mut leaves);
        }
        for (i, l) in leaves.iter().enumerate() {
            let q = l.leaf_named().unwrap();
            if leaves[..i].iter().any(|x| x.leaf_named() == Some(q)) {
                return Err(Error::new(format!(
                    "table name \"{q}\" specified more than once"
                )));
            }
        }
    }
    if let SetExpr::Select(s) = step.query.body.as_mut() {
        starred(&mut s.projection, &from)?;
    }
    let step = &*step;
    let typed = from.typed();
    let mut keyed: Vec<(String, Vec<String>)> = Vec::new();
    let mut joined: Vec<&Expr> = Vec::new();
    {
        let mut leaves = Vec::new();
        for n in &from.items {
            n.leaves(&mut leaves);
        }
        for l in &leaves {
            match &l.what {
                What::Table { table, qualifier } => {
                    let t = &catalog.tables[*table];
                    let primary: Vec<String> = t
                        .orders
                        .iter()
                        .find(|o| o.constraint.as_ref().is_some_and(|k| k.primary))
                        .map(|o| {
                            o.levels
                                .iter()
                                .map(|lv| t.columns[lv.column()].name.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    keyed.push((qualifier.clone(), primary));
                }
                What::Call { call, .. } => joined.push(&call.expr),
                _ => {}
            }
        }
        fn ons<'n>(n: &'n Node, out: &mut Vec<&'n Expr>) {
            if let What::Join {
                left, right, on, ..
            } = &n.what
            {
                ons(left, out);
                ons(right, out);
                if let Some(e) = on {
                    out.push(e);
                }
            }
        }
        for n in &from.items {
            ons(n, &mut joined);
        }
    }
    let mut p = pipeline::prepare(&step.query, &typed, &keyed, release, now, &joined)?;
    // Each column it reads is one of its FROM's, refused before any row as PostgreSQL's parser
    // refuses one it finds none or two of.
    for (q, n) in p.reads() {
        let found = pipeline::naming(
            from.columns
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            q.as_deref(),
            &n,
        );
        match (found.len(), q) {
            (1, _) => {}
            (0, Some(q))
                if !from.columns.iter().any(|c| {
                    c.qualifier.as_deref().map(pipeline::unmarked) == Some(q.as_str())
                }) =>
            {
                return Err(Error::new(format!(
                    "missing FROM-clause entry for table \"{q}\""
                )))
            }
            (0, Some(q)) => return Err(Error::new(format!("column {q}.{n} does not exist"))),
            (0, None) => return Err(Error::new(format!("column \"{n}\" does not exist"))),
            _ => return Err(Error::new(format!("column reference \"{n}\" is ambiguous"))),
        }
    }
    let carried: Vec<(Option<String>, String, Carried)> = from
        .columns
        .iter()
        .map(|c| (c.qualifier.clone(), c.name.clone(), c.carried.clone()))
        .collect();
    p.carried = casings.add_step(&step.name, &p, &carried)?;
    // The FROM's rows.
    // The words it writes, its functions' calls among them.
    let text = std::iter::once(step.query.to_string())
        .chain(step.calls.iter().map(|c| c.expr.to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    let ws = words(&text);
    let mut wheres = Vec::new();
    if let SetExpr::Select(s) = step.query.body.as_ref() {
        if let Some(w) = &s.selection {
            conjuncts(w, &mut wheres);
        }
    }
    let (rows, order) = {
        let mut reading = Reading {
            reads,
            from: &from,
            words: &ws,
            wheres,
            params,
            release,
            casings,
            types: &p.types,
            now,
            made,
            accrued,
            cached: HashMap::new(),
            order: None,
        };
        let mut rows: Vec<Vec<Value>> = vec![Vec::new()];
        let mut order = None;
        for (k, item) in from.items.iter().enumerate() {
            reading.order = None;
            let next = if item.lateral() && k > 0 {
                let mut out = Vec::new();
                for r in &rows {
                    for x in reading.rows(item, Some((0, r)), true)? {
                        let mut row = r.clone();
                        row.extend(x);
                        out.push(row);
                    }
                }
                out
            } else {
                let xs = reading.rows(item, None, true)?;
                let mut out = Vec::with_capacity(rows.len() * xs.len());
                for r in &rows {
                    for x in &xs {
                        let mut row = r.clone();
                        row.extend(x.iter().cloned());
                        out.push(row);
                    }
                }
                out
            };
            // The order holds where the first relation is read alone in it and the rest give one
            // row.
            if k == 0 {
                order = reading.order;
            } else if next.len() != rows.len() {
                order = None;
            }
            rows = next;
        }
        if from.items.is_empty() {
            order = None;
        }
        (rows, order)
    };
    let rel = Relation {
        names: from.columns.iter().map(|c| c.name.clone()).collect(),
        rows,
        order,
    };
    let columns: Columns = from
        .columns
        .iter()
        .map(|c| (c.qualifier.clone(), c.name.clone()))
        .collect();
    let types = p.returns.clone();
    let carried = p.carried.clone();
    let mut run = pipeline::StepRun::new(p);
    let rel = match run.feed(rel, columns, params, release, casings)? {
        Some(out) => out,
        None => run.finish(params, release, casings)?,
    };
    Ok(Made {
        rel,
        types,
        carried,
    })
}

/// Whether a question reads what only the pipeline over several relations reads: no table, or more
/// than one; a view, a FROM item other than a table or a step by name, a join other than CROSS JOIN,
/// a star, or a step's columns named by its alias.
pub(super) fn several(catalog: &Catalog, q: &Query) -> bool {
    let mut tables: Vec<String> = Vec::new();
    let mut steps: Vec<String> = Vec::new();
    let mut beyond = false;
    let mut note = |body: &SetExpr, steps: &[String], tables: &mut Vec<String>| match body {
        SetExpr::Select(s) => {
            beyond |= s.projection.iter().any(|i| {
                matches!(
                    i,
                    SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
                )
            });
            for item in &s.from {
                for (k, t) in std::iter::once(&item.relation)
                    .chain(item.joins.iter().map(|j| &j.relation))
                    .enumerate()
                {
                    if k > 0
                        && !matches!(
                            item.joins[k - 1].join_operator,
                            JoinOperator::CrossJoin(JoinConstraint::None)
                        )
                    {
                        beyond = true;
                    }
                    match t {
                        TableFactor::Table {
                            name, args: None, ..
                        } => {
                            let n = crate::catalog::object_name(name);
                            if steps.contains(&n) {
                                continue;
                            }
                            if catalog.view(&n).is_some() {
                                beyond = true;
                            }
                            if !tables.contains(&n) {
                                tables.push(n);
                            }
                        }
                        _ => beyond = true,
                    }
                }
            }
        }
        _ => beyond = true,
    };
    if let Some(with) = &q.with {
        if with.recursive {
            return false;
        }
        for cte in &with.cte_tables {
            note(cte.query.body.as_ref(), &steps, &mut tables);
            steps.push(crate::catalog::ident(&cte.alias.name));
        }
    }
    note(q.body.as_ref(), &steps, &mut tables);
    let renamed = q
        .with
        .as_ref()
        .is_some_and(|w| w.cte_tables.iter().any(|c| !c.alias.columns.is_empty()));
    beyond || renamed || tables.len() != 1
}

/// Whether a question names a view.
pub(super) fn names_a_view(catalog: &Catalog, q: &Query) -> bool {
    if catalog.views.is_empty() {
        return false;
    }
    let mut q = q.clone();
    let mut found = false;
    super::each_table(&mut q, &mut |t| {
        if let TableFactor::Table { name, .. } = t {
            found |= catalog.view(&crate::catalog::object_name(name)).is_some();
        }
    });
    found
}

/// A catalog whose tables are read as holding no rows: what a view's question is checked over.
struct Empty<'c>(&'c Catalog);

impl Reads for Empty<'_> {
    fn catalog(&self) -> &Catalog {
        self.0
    }

    fn read(&self, q: &Question, _order: Option<&str>) -> Result<(RecordBatch, Round)> {
        let tables: Vec<&Table> = q.sides.iter().map(|s| &self.0.tables[s.table]).collect();
        let batch = super::answer::Answer::new(&q.select)
            .typed(&tables)
            .finish(&tables, &q.select, &q.order)?;
        Ok((batch, Round::default()))
    }

    fn snails(&self) -> usize {
        1
    }
}

/// The names of the columns a question returns, read over a catalog whose tables hold no rows.
pub(super) fn checked(catalog: &Catalog, q: &Query) -> Result<Vec<String>> {
    let (batch, _) = answer(&Empty(catalog), q, &[])?;
    Ok(batch
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().clone())
        .collect())
}

/// Several snails' tables, each read from the snail that holds it: `at` names, for each table of
/// the catalog, its snail and its place there.
pub(super) struct Across<'a> {
    pub snails: Vec<&'a super::Snail>,
    pub at: &'a [(usize, usize)],
    pub catalog: &'a Catalog,
}

impl Reads for Across<'_> {
    fn catalog(&self) -> &Catalog {
        self.catalog
    }

    fn read(&self, q: &Question, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        if let [side] = q.sides.as_slice() {
            let (s, t) = self.at[side.table];
            let mut local = q.clone();
            local.sides[0].table = t;
            let (b, r) = self.snails[s].ask_one(&local, order)?;
            let mut parts = vec![Round::default(); self.snails.len()];
            parts[s] = r.clone();
            let mut r = r;
            r.snails = parts;
            return Ok((b, r));
        }
        let at: Vec<(usize, usize)> = q.sides.iter().map(|s| self.at[s.table]).collect();
        super::join::ask_join_across(&self.snails, &at, q)
    }

    fn snails(&self) -> usize {
        self.snails.len()
    }
}
