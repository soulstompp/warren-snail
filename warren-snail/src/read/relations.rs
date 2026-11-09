//! A question over several relations, read as a pipeline: each step's FROM met as one relation, and
//! each step then read as the pipeline reads a step over one. The tables of a FROM the question path
//! joins are read as C1's round meets them, one round; any other table is read alone, by the
//! conditions of its own the question path reads; and the relations a FROM names (tables, steps, a
//! derived table, VALUES or a view lifted to a step of its own, a function's rows) are met by their
//! ON, USING or NATURAL, a function's rows made for each row of the relations before it.

mod apart;
mod kept;
mod met;
mod recursive;

pub(super) use kept::{Keep, Kept};
pub(super) use recursive::unread_clauses;

use super::collate::Carried;
use super::pipeline::{self, Casings, Columns, Relation, Value, MERGED};
use super::spill::{Chunk, Rows, Run, CHUNK};
use super::question::{self, Question};
use super::typed::{self, TypedColumn};
use super::walk::KeySet;
use super::Round;
use crate::catalog::{Catalog, ColumnType, Table};
use crate::{Error, Result};
use arrow::array::RecordBatch;
use sqlparser::ast::{
    BinaryOperator, Expr, FunctionArg, FunctionArgExpr, FunctionArguments, Ident, JoinConstraint,
    JoinOperator, ObjectName, ObjectNamePart, Query, SelectItem, SetExpr, SetOperator,
    SetQuantifier, TableAlias, TableFactor, TableWithJoins, Value as SqlValue,
};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Where the tables a question reads are read: one snail, or several opened together.
pub(super) trait Reads {
    /// The catalog naming the tables.
    fn catalog(&self) -> &Catalog;
    /// A join the question path reads, over the catalog's tables: one round, each snail's part of it
    /// in its `snails` where there are several, the rows it makes handed to `each` as they are made.
    fn read_join(
        &self,
        q: &Question,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round>;
    /// A question on one table the question path reads, of rows alone, kept to the keys `kept` holds
    /// of its columns where it is given them, those of the rows made before it that it meets: its
    /// rows handed to `each` a block at a time as they are read.
    fn read_each(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
        most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round>;
    /// Whether a question's pages along a strand are fewer than along its order of fewest, with the
    /// sort it saves.
    fn strand_wins(&self, q: &Question, strand: &str) -> Result<bool>;
    /// How many snails the tables lie in.
    fn snails(&self) -> usize;
}

/// The mark a function's place in a FROM is named by, once lifted out of it.
const CALLED: &str = "\0call";

/// The mark a LATERAL subquery's place in a FROM is named by, once lifted out of it.
const LATERAL: &str = "\0lateral";

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
    /// A set operation's arms, where the step is one.
    set: Option<Arm>,
    /// The LATERAL subqueries its FROM reads, each standing in its FROM under LATERAL and its place
    /// here, with what its reads keep between the rows before it.
    laterals: Vec<(Query, super::subqueries::Kept)>,
    /// A recursive WITH query's terms, where the step is one.
    recursion: Option<recursive::Recursion>,
}

/// A function a FROM reads the rows of.
#[derive(Clone)]
struct Call {
    expr: Expr,
    ordinality: bool,
}

/// What a step made: its rows, and the type and the collation of each of its columns.
#[derive(Clone)]
pub(super) struct Made {
    pub(super) rel: Relation,
    pub(super) types: Vec<Option<ColumnType>>,
    carried: Vec<Carried>,
    /// Where the step is kept for the steps after it, its rows in the round's room, those past it
    /// written: `rel` then holds none.
    kept: Option<Rows>,
    /// Each column's refusal where a statement asks it, a catalog table's column only a server
    /// fills: empty where none is refused.
    refused: Vec<Option<String>>,
}

impl Made {
    /// One of pg_catalog's tables as the file's release and its own objects make it, each value
    /// read by its column's type.
    fn of_catalog(c: crate::pgcatalog::Relation) -> Result<Made> {
        let rows = c
            .rows
            .iter()
            .map(|r| {
                r.iter()
                    .zip(&c.types)
                    .map(|(v, &ty)| catalog_value(ty, v.as_deref()))
                    .collect::<Result<Vec<Value>>>()
            })
            .collect::<Result<Vec<_>>>()?;
        let mut m = Made::of(
            Relation {
                names: c.names,
                rows,
                order: None,
            },
            c.types.iter().map(|&t| Some(t)).collect(),
            vec![Carried::None; c.types.len()],
        );
        m.refused = c.refused;
        Ok(m)
    }

    fn of(rel: Relation, types: Vec<Option<ColumnType>>, carried: Vec<Carried>) -> Made {
        Made {
            refused: Vec::new(),
            rel,
            types,
            carried,
            kept: None,
        }
    }

    /// Its rows as a place of their own.
    pub(super) fn into_rows(self) -> Result<Rows> {
        match self.kept {
            Some(run) => Ok(run),
            None => Rows::of(self.rel.rows),
        }
    }

    /// Its rows as a place.
    pub(super) fn rows_of(&self) -> Result<Rows> {
        match &self.kept {
            Some(run) => Ok(run.clone()),
            None => Rows::of(self.rel.rows.clone()),
        }
    }

}

/// A catalog table's value read from its text, as its column's type's input reads it.
fn catalog_value(ty: ColumnType, s: Option<&str>) -> Result<Value> {
    let Some(s) = s else {
        return Ok(Value::Null);
    };
    // A parse tree's cell holds the name of the object it comes from, which pg_get_expr reads.
    if let ColumnType::Internal(crate::textual::Internal::NodeTree) = ty {
        return Ok(Value::Typed(ty, s.to_string()));
    }
    if let ColumnType::Composed(_) = ty {
        let item = crate::composed::text_in(ty, s)?;
        return Ok(Value::Composed(
            ty,
            std::sync::Arc::new(crate::composed::Compound::new(ty, item)),
        ));
    }
    Ok(super::fill::value_of(&crate::composed::scalar_in(ty, s)?, ty))
}

/// The catalog tables a statement names, each made as the file's release and its objects make it,
/// by the names a FROM reads it by: `pg_catalog.x`, and `x` alone, pg_catalog searched first.
fn catalog_steps(catalog: &Catalog, q: &Query) -> Result<Steps> {
    let mut steps = Steps::new();
    let text = q.to_string();
    if !crate::pgcatalog::names_catalog(&text) {
        return Ok(steps);
    }
    let words = super::sysviews::words_of(&text);
    // The tables the statement names itself, then those the views it names read: inside one of
    // PostgreSQL's views a table only a server holds is empty.
    let mut add = |words: &[String], in_view: bool| -> Result<()> {
        for w in words {
            let Some(table) = crate::pgcatalog::table_named(w) else {
                continue;
            };
            if steps.contains_key(table) {
                continue;
            }
            let rel = match in_view {
                true => crate::pgcatalog::relation_in_view(catalog, table)?,
                false => crate::pgcatalog::relation(catalog, table)?,
            };
            let made = Rc::new(Made::of_catalog(rel)?);
            steps.insert(format!("pg_catalog.{table}"), made.clone());
            steps.insert(table.to_string(), made);
        }
        Ok(())
    };
    add(&words, false)?;
    for d in super::sysviews::texts_read(&words, catalog.release) {
        add(&super::sysviews::words_of(&d), true)?;
    }
    Ok(steps)
}

/// The relations a statement made before a step, by the names its FROM reads them by.
pub(super) type Steps = HashMap<String, Rc<Made>>;

/// What every step of a statement reads by: the snail, the parameters, the release whose catalog
/// names a reg* value, and the statement's clock.
#[derive(Clone, Copy)]
pub(super) struct Asked<'a> {
    pub(super) reads: &'a dyn Reads,
    pub(super) params: &'a [String],
    pub(super) release: u8,
    pub(super) now: i64,
    /// Whether what the question returns is read by the statement around it, so it lies in the
    /// round's room.
    pub(super) answer_kept: bool,
}

/// The columns of the queries around a subquery, each qualifier marked once more for each query
/// out, their values on the row it is answered for, and what its reads keep between rows.
pub(super) struct Outer<'o> {
    pub(super) columns: Vec<Col>,
    pub(super) values: Vec<Value>,
    pub(super) kept: &'o super::subqueries::Kept,
}

/// A column of a query, marked as one of the query around a subquery.
pub(super) fn marked(c: &Col) -> Col {
    Col {
        qualifier: Some(format!(
            "{}{}",
            pipeline::OUTER,
            c.qualifier.as_deref().unwrap_or_default()
        )),
        ..c.clone()
    }
}

/// A step's FROM rows read once for every row a subquery is answered for, where they read nothing of
/// the query around it, by the keys of the =s that tie them to it. Where the room holds them, a hash
/// over them; past it, the rows of the keys that hash lowest held and the others written, a pass at
/// a time, where the answers come in that order; else each row that asks reads them a chunk at a
/// time.
pub(super) struct Parted {
    order: Option<usize>,
    held: PartedRows,
}

enum PartedRows {
    Whole {
        rows: Vec<Vec<Value>>,
        index: Option<HashMap<Vec<pipeline::Key>, Vec<usize>>>,
        _held: Option<pipeline::parted::Pass>,
        _counted: crate::room::Holding,
    },
    Passes(std::cell::RefCell<Passes>),
    Scanned(Rows),
}

struct Passes {
    hashed: pipeline::parted::Parted,
    pass: Option<pipeline::parted::Pass>,
    index: HashMap<Vec<pipeline::Key>, Vec<usize>>,
    begun: bool,
}

/// What ties a subquery's FROM rows to the query around it.
struct Tie<'t, 'e> {
    tied: &'t [(&'e Expr, &'e Expr)],
    from: &'t From,
    asked: Asked<'t>,
    casings: &'t Casings,
    types: &'t typed::Types,
}

impl Tie<'_, '_> {
    fn frame(&self) -> pipeline::Frame<'_> {
        pipeline::Frame {
            columns: self.from.frame_columns(0, self.from.columns.len()),
            params: self.asked.params,
            release: self.asked.release,
            casings: self.casings,
            types: self.types,
            now: self.asked.now,
            subqueries: None,
        }
    }

    /// The keys of a FROM row's columns the =s tie; none where one is NULL, which none meets.
    fn of_row(&self, r: &[Value], frame: &pipeline::Frame) -> Result<Option<Vec<pipeline::Key>>> {
        let none = pipeline::computed_none();
        let mut row = r.to_vec();
        row.resize(self.from.columns.len(), Value::Null);
        let mut k = Vec::with_capacity(self.tied.len());
        for (a, _) in self.tied {
            let v = pipeline::eval(a, &row, frame, &none)?;
            if v.is_null() {
                return Ok(None);
            }
            k.push(pipeline::key(&v));
        }
        Ok(Some(k))
    }

    /// The key a set of values the row asking gives asks for; none where one is NULL.
    fn of_values(
        &self,
        values: &[Value],
        frame: &pipeline::Frame,
    ) -> Result<Option<Vec<pipeline::Key>>> {
        let none = pipeline::computed_none();
        let mut row = vec![Value::Null; self.from.outer_at];
        row.extend(values.iter().cloned());
        let mut k = Vec::with_capacity(self.tied.len());
        for (_, b) in self.tied {
            let v = pipeline::eval(b, &row, frame, &none)?;
            if v.is_null() {
                return Ok(None);
            }
            k.push(pipeline::key(&v));
        }
        Ok(Some(k))
    }

    fn hash_of_row(&self, r: &[Value], frame: &pipeline::Frame) -> Result<u64> {
        Ok(self
            .of_row(r, frame)?
            .map_or(0, |k| pipeline::parted::key_hash(&k, 0)))
    }

    /// Puts the sets of values the rows asking give in the order of their keys' hashes.
    fn in_hash_order(&self, o: &Outer) -> Result<()> {
        let frame = self.frame();
        let mut around = o.kept.around.borrow_mut();
        if let Some(sets) = around.as_mut() {
            let mut hashed = Vec::with_capacity(sets.len());
            for set in sets.drain(..) {
                let h = self
                    .of_values(&set, &frame)?
                    .map_or(0, |k| pipeline::parted::key_hash(&k, 0));
                hashed.push((h, set));
            }
            hashed.sort_by_key(|(h, _)| *h);
            sets.extend(hashed.into_iter().map(|(_, s)| s));
        }
        Ok(())
    }
}

impl Parted {
    /// The rows `read` hands on, held as the room lets them be.
    fn read(
        tie: &Tie,
        in_passes: bool,
        rows_known: &std::cell::Cell<Option<u64>>,
        read: impl FnOnce(Each) -> Result<()>,
    ) -> Result<Parted> {
        let mut order = None;
        let frame = tie.frame();
        let index_of = |rows: &[&Vec<Value>]| -> Result<_> {
            let mut index: HashMap<Vec<pipeline::Key>, Vec<usize>> = HashMap::new();
            for (i, r) in rows.iter().enumerate() {
                if let Some(k) = tie.of_row(r, &frame)? {
                    index.entry(k).or_default().push(i);
                }
            }
            Ok(index)
        };
        if in_passes {
            let mut hashed = pipeline::parted::Parted::new();
            read(&mut |c, at| {
                order = at;
                hashed.rows_to_come = rows_known.get();
                for r in c {
                    hashed.push(tie.hash_of_row(&r, &frame)?, r)?;
                }
                Ok(())
            })?;
            let pass = hashed.pass(&mut |r| tie.hash_of_row(r, &frame))?;
            let held = match pass {
                Some(mut p) if p.below.is_none() => {
                    let rows: Vec<Vec<Value>> =
                        std::mem::take(&mut p.rows).into_iter().map(|(_, r)| r).collect();
                    let refs: Vec<&Vec<Value>> = rows.iter().collect();
                    let index = Some(index_of(&refs)?);
                    PartedRows::Whole {
                        rows,
                        index,
                        _held: Some(p),
                        _counted: crate::room::holding(0),
                    }
                }
                None => PartedRows::Whole {
                    rows: Vec::new(),
                    index: Some(HashMap::new()),
                    _held: None,
                    _counted: crate::room::holding(0),
                },
                pass => {
                    let index = match &pass {
                        Some(p) => {
                            let refs: Vec<&Vec<Value>> = p.rows.iter().map(|(_, r)| r).collect();
                            index_of(&refs)?
                        }
                        None => HashMap::new(),
                    };
                    PartedRows::Passes(std::cell::RefCell::new(Passes {
                        hashed,
                        pass,
                        index,
                        begun: true,
                    }))
                }
            };
            return Ok(Parted { order, held });
        }
        let run = Run::new();
        read(&mut |c, at| {
            order = at;
            c.into_iter().try_for_each(|r| run.push(r))
        })?;
        run.seal()?;
        let rows = Rows::one(run);
        if !rows.held_whole() {
            return Ok(Parted {
                order,
                held: PartedRows::Scanned(rows),
            });
        }
        let index = match tie.tied.is_empty() {
            true => None,
            false => Some(rows.with_held(|r| index_of(r))?),
        };
        let all = rows.all()?;
        let _counted = crate::room::holding(super::spill::rows_width(&all));
        Ok(Parted {
            order,
            held: PartedRows::Whole {
                rows: all,
                index,
                _held: None,
                _counted,
            },
        })
    }

    fn in_passes(&self) -> bool {
        matches!(self.held, PartedRows::Passes(_))
    }

    /// Hands `feed` the rows a row of the query around it meets, where it gives `o`'s values.
    fn rows_for(
        &self,
        tie: &Tie,
        o: &Outer,
        feed: &mut dyn FnMut(Vec<Vec<Value>>, Option<usize>) -> Result<()>,
    ) -> Result<()> {
        let frame = tie.frame();
        let asked = match tie.tied.is_empty() {
            true => None,
            false => Some(tie.of_values(&o.values, &frame)?),
        };
        match &self.held {
            PartedRows::Whole { rows, index, .. } => match (index, asked) {
                (Some(index), Some(k)) => {
                    let found = k.and_then(|k| index.get(&k));
                    let rows = found.map_or(Vec::new(), |at| {
                        at.iter().map(|&i| rows[i].clone()).collect()
                    });
                    feed(rows, self.order)
                }
                _ => feed(rows.clone(), self.order),
            },
            PartedRows::Passes(p) => {
                let Some(k) = asked.flatten() else {
                    return feed(Vec::new(), self.order);
                };
                let hash = pipeline::parted::key_hash(&k, 0);
                let mut p = p.borrow_mut();
                let p = &mut *p;
                while p.begun
                    && p.pass
                        .as_ref()
                        .is_some_and(|x| x.below.is_some_and(|b| hash >= b))
                {
                    p.pass = None;
                    p.pass = p.hashed.pass(&mut |r| tie.hash_of_row(r, &frame))?;
                    p.index.clear();
                    if let Some(x) = &p.pass {
                        for (i, (_, r)) in x.rows.iter().enumerate() {
                            if let Some(k) = tie.of_row(r, &frame)? {
                                p.index.entry(k).or_default().push(i);
                            }
                        }
                    }
                }
                let rows = match (&p.pass, p.index.get(&k)) {
                    (Some(x), Some(at)) => at.iter().map(|&i| x.rows[i].1.clone()).collect(),
                    _ => Vec::new(),
                };
                feed(rows, self.order)
            }
            PartedRows::Scanned(rows) => {
                let order = self.order;
                rows.each_chunk(&mut |c| match &asked {
                    None => feed(c, order),
                    Some(None) => Ok(()),
                    Some(Some(k)) => {
                        let mut met = Vec::new();
                        for r in c {
                            if tie.of_row(&r, &frame)?.as_ref() == Some(k) {
                                met.push(r);
                            }
                        }
                        feed(met, order)
                    }
                })
            }
        }
    }
}

/// The steps a question makes, in the order they are read: each WITH query, each relation lifted
/// out of a FROM before the step that reads it, and the question last. A WITH inside a step is
/// read as steps of the question's own, under names kept apart (`apart`).
fn steps(catalog: &Catalog, q: &Query, before: &[String]) -> Result<Vec<Step>> {
    let mut q = q.clone();
    apart::below(&mut q)?;
    let mut out = Vec::new();
    let mut visible: Vec<String> = before.to_vec();
    if let Some(with) = q.with.take() {
        with_steps(catalog, &mut out, &mut visible, &with)?;
    }
    lift(catalog, &mut out, &visible, String::new(), q, Vec::new())?;
    Ok(out)
}

/// A WITH's queries as steps, each visible to the steps lifted after it.
fn with_steps(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &mut Vec<String>,
    with: &sqlparser::ast::With,
) -> Result<()> {
    // Under RECURSIVE each query reads any other, so they are made in the order they read each
    // other, a query reading itself as its recursion.
    let ordered = match with.recursive {
        true => recursive::ordered(&with.cte_tables)?,
        false => with.cte_tables.iter().map(|c| (c, false)).collect(),
    };
    for (cte, recursion) in ordered {
        let name = crate::catalog::ident(&cte.alias.name);
        let renames = cte
            .alias
            .columns
            .iter()
            .map(|c| crate::catalog::ident(&c.name))
            .collect();
        let query = (*cte.query).clone();
        match recursion {
            true => recursive::lift(catalog, out, &visible[..], name.clone(), query, renames)?,
            false => lift(catalog, out, &visible[..], name.clone(), query, renames)?,
        }
        visible.push(name);
    }
    Ok(())
}

/// Whether a WITH query writes: INSERT, UPDATE, DELETE or MERGE.
fn writes(q: &Query) -> bool {
    matches!(
        q.body.as_ref(),
        SetExpr::Insert(_) | SetExpr::Update(_) | SetExpr::Delete(_) | SetExpr::Merge(_)
    )
}

/// One step, after the relations its FROM lifts out of itself, and the steps its own WITH makes
/// before it.
fn lift(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    name: String,
    mut query: Query,
    renames: Vec<String>,
) -> Result<()> {
    let seen;
    let visible = match query.with.is_some() {
        false => visible,
        true => {
            // A view's question comes here as written, so its names are kept apart here.
            apart::all(&mut query)?;
            let with = query.with.take().expect("a WITH");
            if with.cte_tables.iter().any(|c| writes(&c.query)) {
                return Err(Error::new(
                    "WITH clause containing a data-modifying statement must be at the top level",
                ));
            }
            let mut v = visible.to_vec();
            with_steps(catalog, out, &mut v, &with)?;
            seen = v;
            &seen[..]
        }
    };
    let mut lifted = Lifted::default();
    match query.body.as_mut() {
        SetExpr::Select(s) => {
            for item in &mut s.from {
                lift_join(catalog, out, visible, item, &mut lifted)?;
            }
        }
        SetExpr::Values(_) => {
            if query.order_by.is_some() || query.limit_clause.is_some() || query.fetch.is_some() {
                return Err(Error::new("a VALUES list sorted or limited is not read"));
            }
        }
        SetExpr::SetOperation { .. } => {
            return lift_set(catalog, out, visible, name, query, renames)
        }
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
        calls: lifted.calls,
        set: None,
        laterals: lifted
            .laterals
            .into_iter()
            .map(|q| (q, Default::default()))
            .collect(),
        recursion: None,
    });
    Ok(())
}

/// What a FROM keeps beside its step once lifted out of it: the functions it calls and the LATERAL
/// subqueries it reads.
#[derive(Default)]
struct Lifted {
    calls: Vec<Call>,
    laterals: Vec<Query>,
}

fn lift_join(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    item: &mut TableWithJoins,
    lifted: &mut Lifted,
) -> Result<()> {
    lift_factor(catalog, out, visible, &mut item.relation, lifted)?;
    for j in &mut item.joins {
        lift_factor(catalog, out, visible, &mut j.relation, lifted)?;
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
    lifted: &mut Lifted,
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
            sample,
            index_hints,
            ..
        } if with_hints.is_empty() && partitions.is_empty() && index_hints.is_empty() => {
            let n = crate::catalog::object_name(name);
            if sample.is_some() && (visible.contains(&n) || catalog.view(&n).is_some()) {
                return Err(Error::new(
                    "TABLESAMPLE clause can only be applied to tables and materialized views",
                ));
            }
            if visible.contains(&n) {
                return Ok(());
            }
            // One of PostgreSQL's views, pg_catalog searched first.
            if let Some((q, refused)) = super::sysviews::system_view(catalog, &n)? {
                let step = fresh(out, "view");
                lift(catalog, out, &[], step.clone(), q, Vec::new())?;
                super::sysviews::note_refused(&step, refused);
                let bare = n.rsplit('.').next().unwrap_or(&n).to_string();
                let kept = alias.take().unwrap_or_else(|| alias_named(&bare));
                *t = plain(&step, Some(kept));
                return Ok(());
            }
            if let Some(v) = catalog.view(&n) {
                let bound = crate::grammar::bound(&v.query);
                let parsed = super::image::parse_sql(bound.as_deref().unwrap_or(&v.query))?;
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
        TableFactor::Derived {
            lateral: true,
            subquery,
            alias,
            sample: None,
        } => {
            lifted.laterals.push((**subquery).clone());
            let at = lifted.laterals.len() - 1;
            *t = plain(&format!("{LATERAL}{at}"), alias.take());
            return Ok(());
        }
        TableFactor::NestedJoin {
            table_with_joins,
            alias: None,
        } => return lift_join(catalog, out, visible, table_with_joins, lifted),
        other => return Err(Error::new(format!("{other} is not read in FROM"))),
    };
    lifted.calls.push(Call { expr, ordinality });
    *t = plain(&format!("{CALLED}{}", lifted.calls.len() - 1), alias);
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
pub(super) struct Col {
    pub(super) qualifier: Option<String>,
    pub(super) name: String,
    pub(super) ty: Option<ColumnType>,
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
    /// A table of the snail, by its place in the catalog, and the TABLESAMPLE it is read under.
    Table {
        table: usize,
        qualifier: String,
        sample: Option<String>,
    },
    /// A step made before.
    Made { name: String, qualifier: String },
    /// A function's rows, made for each row of the relations before it.
    Call {
        call: Call,
        qualifier: String,
        /// Whether its record is spread into its fields, one field among them.
        spread: bool,
    },
    /// A LATERAL subquery's rows, by its place among the step's, made for each row of the relations
    /// before it.
    Lateral { at: usize, qualifier: String },
    Join {
        left: Box<Node>,
        right: Box<Node>,
        kind: Kind,
        on: Option<Expr>,
        merged: Vec<Merged>,
    },
}

impl Node {
    /// Whether a USING or NATURAL of this node, or of one under it, meets the column at a place.
    fn meets(&self, place: usize) -> bool {
        match &self.what {
            What::Join {
                left,
                right,
                merged,
                ..
            } => {
                merged.iter().any(|m| m.left == place || m.right == place)
                    || left.meets(place)
                    || right.meets(place)
            }
            _ => false,
        }
    }

    /// The relation a qualified star or a single name reads, where the node is one.
    fn leaf_named(&self) -> Option<&str> {
        match &self.what {
            What::Table { qualifier, .. }
            | What::Made { qualifier, .. }
            | What::Call { qualifier, .. }
            | What::Lateral { qualifier, .. } => Some(qualifier),
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

    /// Whether its rows are made for each row before it: its leftmost relation is a function's or a
    /// LATERAL subquery's.
    fn lateral(&self) -> bool {
        match &self.what {
            What::Call { .. } | What::Lateral { .. } => true,
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
    /// Where the columns of the query around it begin, where it is a subquery's: after its own.
    outer_at: usize,
    /// The refusal of each column a statement may not ask, by its place: a catalog table's that only
    /// a server fills.
    refused: HashMap<usize, String>,
}

impl From {
    /// The refusal of a qualifier `q` no relation of the FROM answers to, in PostgreSQL's words:
    /// an invalid reference where it names a relation its alias hides (a table by its schema and
    /// name, or by its name alone where the search path finds it; a WITH query by its name), else a
    /// missing entry.
    fn no_entry(&self, catalog: &Catalog, q: &str) -> Error {
        let mut leaves = Vec::new();
        for n in &self.items {
            n.leaves(&mut leaves);
        }
        let hidden = leaves.iter().any(|l| match &l.what {
            What::Table {
                table, qualifier, ..
            } => {
                let name = &catalog.tables[*table].name;
                qualifier != q && crate::catalog::found_by_qualifier(name, q)
            }
            What::Made { name, qualifier } => qualifier != q && apart::written(name) == q,
            _ => false,
        });
        let relation = q.rsplit('.').next().unwrap_or(q);
        Error::new(match hidden {
            true => format!("invalid reference to FROM-clause entry for table \"{relation}\""),
            false => format!("missing FROM-clause entry for table \"{relation}\""),
        })
    }

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

/// What laying a FROM out reads: the catalog, the steps made before, the step's functions and
/// LATERAL subqueries, and the columns of the query around it, where it is a subquery's.
struct Laying<'a> {
    catalog: &'a Catalog,
    made: &'a Steps,
    calls: &'a [Call],
    laterals: &'a [(Query, super::subqueries::Kept)],
    asked: Asked<'a>,
    casings: &'a Casings,
    outer: Option<&'a Outer<'a>>,
    release: u8,
    columns: Vec<Col>,
    /// Whether the step names a system column, which each table of it then holds past its own.
    systems: bool,
    /// The refusal of each column of the FROM a statement may not ask, by its place.
    refused: HashMap<usize, String>,
}

impl Laying<'_> {
    fn factor(&mut self, t: &TableFactor) -> Result<Node> {
        let at = self.columns.len();
        let TableFactor::Table {
            name,
            alias,
            sample,
            ..
        } = t
        else {
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
        if let Some(i) = n.strip_prefix(LATERAL) {
            return self.lateral(i.parse::<usize>().unwrap(), alias, renames);
        }
        let (what, cols): (What, Vec<Col>) = if let Some(m) = self.made.get(&n) {
            let q = qualifier(&n);
            // A catalog table's refusals, or those of the view a step was lifted from.
            let refusals = match m.refused.is_empty() {
                true => super::sysviews::refused_of(&n),
                false => m.refused.clone(),
            };
            for (i, r) in refusals.iter().enumerate() {
                if let Some(r) = r {
                    self.refused.insert(at + i, r.clone());
                }
            }
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
                    sample: sample.as_ref().map(|k| match k {
                        sqlparser::ast::TableSampleKind::AfterTableAlias(s)
                        | sqlparser::ast::TableSampleKind::BeforeTableAlias(s) => s.to_string(),
                    }),
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
        let starred = cols.len();
        // A table's system columns, past its own, in no star.
        if let (true, What::Table { qualifier, .. }) = (self.systems, &what) {
            for (name, ty) in [("ctid", ColumnType::Tid), ("tableoid", ColumnType::Oid)] {
                cols.push(Col {
                    qualifier: Some(qualifier.clone()),
                    name: name.to_string(),
                    ty: Some(ty),
                    carried: Carried::None,
                });
            }
        }
        let width = cols.len();
        self.columns.extend(cols);
        Ok(Node {
            what,
            at,
            width,
            star: (at..at + starred).collect(),
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
        if !pipeline::is_set_returning(&name) && !pipeline::gives_a_row(f) {
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
        let spread = fields.is_some();
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
        // A plan cache's counts, which a snail has none of, refused by name where a statement
        // reads them.
        if name == "pg_prepared_statement" {
            for (i, c) in cols.iter().enumerate() {
                if super::prepared::PLAN_CACHE.contains(&c.name.as_str()) {
                    self.refused
                        .insert(at + i, super::prepared::plan_cache_refusal(&c.name));
                }
            }
        }
        for (c, r) in cols.iter_mut().zip(&renames) {
            c.name = r.clone();
        }
        let width = cols.len();
        self.columns.extend(cols);
        Ok(Node {
            what: What::Call {
                call,
                qualifier: q,
                spread,
            },
            at,
            width,
            star: (at..at + width).collect(),
        })
    }

    /// A LATERAL subquery's rows: the columns its question returns, read over tables holding no
    /// rows with the columns laid before it, and the query's around the step, as the query around
    /// it.
    fn lateral(
        &mut self,
        at: usize,
        alias: &Option<TableAlias>,
        renames: Vec<String>,
    ) -> Result<Node> {
        let place = self.columns.len();
        let kept = super::subqueries::Kept::default();
        let mut columns: Vec<Col> = self.columns.iter().map(marked).collect();
        if let Some(o) = self.outer {
            columns.extend(o.columns.iter().map(marked));
        }
        let outer = Outer {
            values: vec![Value::Null; columns.len()],
            columns,
            kept: &kept,
        };
        let empty = Empty(self.catalog);
        let asked = Asked {
            reads: &empty,
            ..self.asked
        };
        let (m, _) = made_by(
            asked,
            &self.laterals[at].0,
            self.made,
            self.casings,
            Some(&outer),
            &Accrued::default(),
        )?;
        let qualifier = alias
            .as_ref()
            .map(|a| crate::catalog::ident(&a.name))
            .unwrap_or_else(|| format!("\u{3}lateral{at}"));
        let mut cols: Vec<Col> = m
            .rel
            .names
            .iter()
            .zip(&m.types)
            .zip(&m.carried)
            .map(|((n, t), k)| Col {
                qualifier: Some(qualifier.clone()),
                name: n.clone(),
                ty: *t,
                carried: k.clone(),
            })
            .collect();
        if renames.len() > cols.len() {
            return Err(Error::new(format!(
                "table \"{qualifier}\" has {} columns available but {} columns specified",
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
            what: What::Lateral { at, qualifier },
            at: place,
            width,
            star: (place..place + width).collect(),
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

/// Whether an expression is a ctid, alone or qualified.
fn is_ctid(e: &Expr) -> bool {
    match e {
        Expr::Identifier(i) => crate::catalog::ident(i) == "ctid",
        Expr::CompoundIdentifier(p) => p.last().is_some_and(|i| crate::catalog::ident(i) == "ctid"),
        Expr::Nested(x) => is_ctid(x),
        _ => false,
    }
}

/// A comparison of a ctid with a value from outside the statement: a literal, a parameter, or
/// anything that reads no ctid and is no subquery of the statement's own. A ctid is a row's address
/// in the generation its statement reads, and a prune moves a live row to another block, where
/// PostgreSQL's VACUUM never moves one: a ctid kept from an earlier statement names another row.
fn ctid_from_outside(e: &Expr) -> Option<String> {
    use sqlparser::ast::BinaryOperator as B;
    let inside = |x: &Expr| {
        let mut found = matches!(x, Expr::Subquery(_));
        crate::ddl::each_expr(x, &mut |y| found |= is_ctid(y) || matches!(y, Expr::Subquery(_)));
        found
    };
    let mut bad = None;
    crate::ddl::each_expr(e, &mut |x| {
        let sides: Vec<&Expr> = match x {
            Expr::BinaryOp {
                left,
                op: B::Eq | B::NotEq | B::Lt | B::LtEq | B::Gt | B::GtEq,
                right,
            } => vec![left, right],
            Expr::InList { expr, list, .. } => std::iter::once(expr.as_ref()).chain(list).collect(),
            Expr::Between {
                expr, low, high, ..
            } => vec![expr, low, high],
            _ => return,
        };
        let with = sides.iter().filter(|s| inside(s)).count();
        if sides.iter().any(|s| is_ctid(s)) && with < sides.len() && bad.is_none() {
            bad = Some(x.to_string());
        }
    });
    bad
}

/// The columns a node of a table holds, by its places: those it stores, then the system columns a
/// step names, where the node is wider than those it stores.
pub(super) fn node_columns(t: &Table, width: usize) -> Vec<usize> {
    let mut c: Vec<usize> = stored(t).collect();
    if width > c.len() {
        c.extend([crate::catalog::CTID, crate::catalog::TABLEOID]);
    }
    c
}

/// The columns a table stores: a step computes each virtual one from them.
pub(super) fn stored(table: &Table) -> impl Iterator<Item = usize> + '_ {
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
pub(super) fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A table's name as SQL writes it, each part quoted.
pub(super) fn quote_name(s: &str) -> String {
    s.split('.').map(quote).collect::<Vec<_>>().join(".")
}

/// The conjuncts of a condition.
pub(super) fn conjuncts<'e>(e: &'e Expr, out: &mut Vec<&'e Expr>) {
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

/// A word as a name: a bare word folded, a quoted one as it is.
fn folded(w: &sqlparser::tokenizer::Word) -> String {
    match w.quote_style {
        Some(_) => w.value.clone(),
        None => w.value.to_lowercase(),
    }
}

/// The words of a text as sqlparser's tokenizer reads them: a bare word folded, a quoted one as it
/// is.
pub(super) fn words(text: &str) -> Vec<String> {
    sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, text)
        .tokenize()
        .unwrap_or_default()
        .into_iter()
        .filter_map(|t| match t {
            sqlparser::tokenizer::Token::Word(w) => Some(folded(&w)),
            sqlparser::tokenizer::Token::Placeholder(p) => Some(p),
            _ => None,
        })
        .collect()
}

/// The columns a text can mean, as sqlparser's tokenizer reads it.
#[derive(Default)]
pub(super) struct Named {
    /// Each name written alone, or after anything but a relation's name: any relation's of that
    /// name. A label after AS and a relation's name before a dot are none.
    bare: HashSet<String>,
    /// Each name written after a relation's name: that relation's alone.
    qualified: HashMap<String, HashSet<String>>,
    /// Each relation written with a star: every column of it.
    starred: HashSet<String>,
    /// The tokenizer read none of it: any column.
    unread: bool,
}

impl Named {
    pub(super) fn of(text: &str) -> Named {
        use sqlparser::keywords::Keyword;
        use sqlparser::tokenizer::Token;
        let mut named = Named::default();
        let Ok(tokens) =
            sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, text)
                .tokenize()
        else {
            named.unread = true;
            return named;
        };
        let tokens: Vec<Token> = tokens
            .into_iter()
            .filter(|t| !matches!(t, Token::Whitespace(_)))
            .collect();
        for (i, t) in tokens.iter().enumerate() {
            let before = |n: usize| i.checked_sub(n).map(|k| &tokens[k]);
            let dotted = matches!(before(1), Some(Token::Period));
            let relation = match before(2) {
                Some(Token::Word(q)) if dotted => Some(folded(q)),
                _ => None,
            };
            match (t, relation) {
                (Token::Mul, Some(q)) => {
                    named.starred.insert(q);
                }
                (Token::Word(w), Some(q)) => {
                    named.qualified.entry(q).or_default().insert(folded(w));
                }
                (Token::Word(w), None) => {
                    let label = matches!(before(1), Some(Token::Word(x))
                        if x.quote_style.is_none() && x.keyword == Keyword::AS);
                    let qualifier = matches!(tokens.get(i + 1), Some(Token::Period));
                    if !label && !qualifier {
                        named.bare.insert(folded(w));
                    }
                }
                _ => {}
            }
        }
        named
    }

    /// Whether the text can mean a column of this name of the relation going by `qualifier`.
    pub(super) fn names(&self, qualifier: Option<&str>, name: &str) -> bool {
        self.unread
            || self.bare.contains(name)
            || qualifier.map(pipeline::unmarked).is_some_and(|q| {
                self.starred.contains(q) || self.qualified.get(q).is_some_and(|n| n.contains(name))
            })
    }
}

/// The question the question path reads: the select list, the FROM and as many of the conditions as
/// it reads together; with none, where it reads them not.
pub(super) fn parsed(
    catalog: &Catalog,
    select: &str,
    from: &str,
    conds: &[&Expr],
) -> Result<Question> {
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

/// What a place hands its rows on to, a chunk at a time, with the column holding its table's order
/// where they keep it.
type Each<'e> = &'e mut dyn FnMut(Vec<Vec<Value>>, Option<usize>) -> Result<()>;

/// A step's FROM read into its rows, as the round reads the tables in it.
struct Reading<'a> {
    reads: &'a dyn Reads,
    from: &'a From,
    /// What the step writes: a column it cannot mean is not read.
    named: &'a Named,
    /// The conjuncts of its WHERE, which a table read alone or a join read as C1's takes where it
    /// reads them.
    wheres: Vec<&'a Expr>,
    params: &'a [String],
    release: u8,
    casings: &'a Casings,
    types: &'a typed::Types,
    now: i64,
    made: &'a Steps,
    accrued: &'a Accrued,
    /// What a LATERAL subquery reads by, its question and what it keeps, and the query around the
    /// step, where it is a subquery's.
    asked: Asked<'a>,
    laterals: &'a [(Query, super::subqueries::Kept)],
    outer: Option<&'a Outer<'a>>,
    /// What a node's read made, where it is asked again for the rows before a function or a LATERAL
    /// subquery.
    cached: HashMap<*const Node, Rows>,
    asked_again: bool,
    /// Whether the rows a node makes keep their table's order, at a column of theirs.
    order: Option<usize>,
    /// What rows made before a table hold of the columns it meets them on, which its read is kept
    /// to.
    kept: Vec<Keep>,
    /// Whether the step's rows come in a strand's order: told none where a join loses it.
    in_order: Option<&'a std::cell::Cell<Option<super::strands::Led>>>,
    /// The strand the FROM's first table is read along, where the step's rows come in its order.
    along: Option<Along>,
    /// How many rows the step's FROM gives, told where it is one relation, and that relation.
    rows_known: &'a std::cell::Cell<Option<u64>>,
    whole_from: Option<*const Node>,
}

/// A FROM's first table read along a strand: its node, the strand, the places in its rows of the
/// columns the strand leads with, and what tells the step its rows come so.
struct Along {
    node: *const Node,
    strand: String,
    at: Vec<usize>,
}

/// What the rounds a statement reads have read, in all and on each snail.
#[derive(Default)]
pub(super) struct Accrued {
    round: std::cell::RefCell<Round>,
    parts: std::cell::RefCell<Vec<Round>>,
}

impl Accrued {
    /// A round's reads added; a question read before and kept, read along no order, adds nothing.
    fn add(&self, r: &Round) {
        if r.order.is_empty() {
            return;
        }
        let order = r.order.clone();
        let mut round = self.round.borrow_mut();
        round.add_table(r, &order);
        round.reads.push(r.clone());
        round.rows_walked += r.rows_walked;
        round.rows_taken += r.rows_taken;
        let mut parts = self.parts.borrow_mut();
        if parts.len() < r.snails.len() {
            parts.resize(r.snails.len(), Round::default());
        }
        for (p, s) in parts.iter_mut().zip(&r.snails) {
            let order = s.order.clone();
            p.add_table(s, &order);
            p.rows_walked += s.rows_walked;
            p.rows_taken += s.rows_taken;
        }
    }
}

impl Reading<'_> {
    /// Whether a column at a place is one the step can mean, by its name or by its place, or one a
    /// USING or NATURAL meets.
    fn needed(&self, place: usize) -> bool {
        let c = &self.from.columns[place];
        self.named.names(c.qualifier.as_deref(), &c.name)
            || self.named.names(None, &format!("{MERGED}{place}"))
            || self.from.items.iter().any(|n| n.meets(place))
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

    /// A node's rows handed to `each` a chunk at a time, each row its `width` values, made with the
    /// row before it where it is lateral: `before` the values of the FROM's columns from `start` to
    /// the node.
    fn flow(
        &mut self,
        node: &Node,
        before: Option<(usize, &[Value])>,
        preserved: bool,
        each: Each,
    ) -> Result<()> {
        if let Some(r) = self.cached.get(&(node as *const Node)).cloned() {
            return r.each_chunk(&mut |c| each(c, None));
        }
        match &node.what {
            What::Table { table, qualifier, .. } => {
                self.alone(node, *table, qualifier, preserved, each)
            }
            What::Made { name, .. } => {
                let rows = self.made[name].rows_of()?;
                if self.whole_from.is_some_and(|n| std::ptr::eq(n, node)) {
                    self.rows_known.set(Some(rows.len() as u64));
                }
                rows.each_chunk(&mut |c| each(c, None))
            }
            What::Call { call, spread, .. } => {
                let rows = self.called(node, call, *spread, before)?;
                each(rows, None)
            }
            What::Lateral { at, .. } => self
                .laterally(node, *at, before)?
                .each_chunk(&mut |c| each(c, None)),
            What::Join { .. } => self.joined(node, before, preserved, each),
        }
    }

    /// A node's rows as a place, in the round's room, those past it written. A node asked again for
    /// the rows before a function or a LATERAL subquery is read once.
    fn place(
        &mut self,
        node: &Node,
        before: Option<(usize, &[Value])>,
        preserved: bool,
    ) -> Result<Rows> {
        if let Some(r) = self.cached.get(&(node as *const Node)) {
            return Ok(r.clone());
        }
        if let What::Made { name, .. } = &node.what {
            return self.made[name].rows_of();
        }
        let run = Run::new();
        let mut order = None;
        self.flow(node, before, preserved, &mut |rows, at| {
            order = at;
            rows.into_iter().try_for_each(|r| run.push(r))
        })?;
        run.seal()?;
        self.order = order;
        let rows = Rows::one(run);
        if self.asked_again && !node.lateral() {
            self.cached.insert(node, rows.clone());
        } else {
            rows.last_reader();
        }
        Ok(rows)
    }

    /// A join's rows, handed to `each` as they are met.
    fn joined(
        &mut self,
        node: &Node,
        before: Option<(usize, &[Value])>,
        preserved: bool,
        each: Each,
    ) -> Result<()> {
        let What::Join {
            left,
            right,
            kind,
            on,
            merged,
        } = &node.what
        else {
            unreachable!("a join")
        };
        // A join's rows keep no table's order.
        self.order = None;
        // Its first table read along a strand, its rows keep the strand's order while the side a
        // hash holds fits the room and the rows that side leaves out make no rows of their own.
        if self.along.is_some() && !right.lateral() {
            let rrows = self.place(right, before, matches!(kind, Kind::Inner | Kind::Cross) && preserved)?;
            if rrows.held_whole() {
                self.order = None;
                let mut hand = |c: Vec<Vec<Value>>| each(c, None);
                return self.met_flowing(
                    node,
                    left,
                    right,
                    *kind,
                    on.as_ref(),
                    merged,
                    rrows,
                    before,
                    preserved,
                    &mut hand,
                );
            }
            self.along = None;
            if let Some(led) = self.in_order {
                led.set(None);
            }
            let keep_left = matches!(kind, Kind::Inner | Kind::Cross | Kind::Left) && preserved;
            let lrows = self.place(left, before, keep_left)?;
            let mut hand = |c: Vec<Vec<Value>>| each(c, None);
            return self.met(
                node,
                left,
                right,
                *kind,
                on.as_ref(),
                merged,
                lrows,
                Some(rrows),
                before,
                &mut hand,
            );
        }
        if self.together(node, preserved, &mut |c| each(c, None))? {
            return Ok(());
        }
        let keep = |side: bool| match kind {
            Kind::Inner | Kind::Cross => preserved,
            Kind::Left => preserved && side,
            Kind::Right => preserved && !side,
            Kind::Full => false,
        };
        // A table met with rows made before it is kept to their keys: the side no join
        // keeps whole, made after the other side; the table, where only one side is one.
        let rlateral = right.lateral();
        let table = |n: &Node| matches!(n.what, What::Table { .. });
        let left_first = !(table(left) && !table(right))
            || !matches!(kind, Kind::Inner | Kind::Right)
            || rlateral;
        let (lrows, rrows) = match left_first {
            true => {
                let lrows = self.place(left, before, keep(true))?;
                let rrows = match rlateral {
                    true => None,
                    false => {
                        let k = match (table(right), kind) {
                            (true, Kind::Inner | Kind::Left) => self.keep_for_join(
                                node,
                                on.as_ref(),
                                merged,
                                right,
                                &lrows,
                                left.at,
                            )?,
                            _ => None,
                        };
                        let pushed = k.is_some();
                        self.kept.extend(k);
                        let rrows = self.place(right, before, keep(false));
                        if pushed {
                            self.kept.pop();
                        }
                        Some(rrows?)
                    }
                };
                (lrows, rrows)
            }
            false => {
                let rrows = self.place(right, before, keep(false))?;
                let k = self.keep_for_join(node, on.as_ref(), merged, left, &rrows, right.at)?;
                let pushed = k.is_some();
                self.kept.extend(k);
                let lrows = self.place(left, before, keep(true));
                if pushed {
                    self.kept.pop();
                }
                (lrows?, Some(rrows))
            }
        };
        self.order = None;
        let mut hand = |c: Vec<Vec<Value>>| each(c, None);
        self.met(
            node,
            left,
            right,
            *kind,
            on.as_ref(),
            merged,
            lrows,
            rrows,
            before,
            &mut hand,
        )
    }

    /// A table read alone, along the order whose pages are fewest, by the conditions of WHERE on
    /// it alone the question path reads, where nothing may leave it NULL: its rows handed to `each` a
    /// block at a time, each row its columns in place, NULL where the step names none.
    fn alone(
        &mut self,
        node: &Node,
        table: usize,
        qualifier: &str,
        preserved: bool,
        each: Each,
    ) -> Result<()> {
        let catalog = self.reads.catalog();
        let t = &catalog.tables[table];
        // A sample is drawn from the whole table: no condition, key or strand narrows its read.
        let sample = match &node.what {
            What::Table { sample, .. } => sample.clone(),
            _ => None,
        };
        let keep = match sample {
            Some(_) => None,
            None => self.keep_of(node).cloned(),
        };
        let mut wanted: Vec<usize> = (node.at..node.at + node.width)
            .filter(|&p| self.needed(p) || keep.as_ref().is_some_and(|k| k.places.contains(&p)))
            .collect();
        if wanted.is_empty() {
            wanted.push(node.at);
        }
        let names: Vec<String> = node_columns(t, node.width)
            .into_iter()
            .map(|c| t.column_at(c).name.clone())
            .collect();
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
        let from = match &sample {
            Some(s) => format!("{} {} {s}", quote_name(&t.name), quote(qualifier)),
            None => format!("{} {}", quote_name(&t.name), quote(qualifier)),
        };
        let conds: Vec<&Expr> = match preserved && sample.is_none() {
            true => self.own_wheres(node),
            false => Vec::new(),
        };
        let q = self.parsed(&select, &from, &conds)?;
        // Kept to the keys of the rows made before it, its read is priced once they are known, and
        // only the rows that meet them are held.
        let kept = keep.as_ref().map(|k| kept::Kept::of(k, node.at, t));
        // Its rows in the table's order, where that order is one unique column it holds: a window
        // reads along it.
        let strand = self
            .along
            .as_ref()
            .filter(|a| sample.is_none() && std::ptr::eq(a.node, node))
            .map(|a| (a.strand.clone(), a.at.clone()));
        let at = along(t)
            .and_then(|c| stored(t).position(|x| x == c))
            .filter(|p| strand.is_none() && wanted.contains(&(node.at + p)));
        self.order = at.map(|p| node.at + p);
        let order = self.order;
        let width = node.width;
        let told = std::cell::Cell::new(None);
        let whole_from = self.whole_from.is_some_and(|n| std::ptr::eq(n, node));
        let rows_known = self.rows_known;
        let round = self.reads.read_each(
            &q,
            strand.as_ref().map(|s| s.0.as_str()),
            kept.as_ref().and_then(|k| k.set.as_ref()),
            if whole_from { rows_known } else { &told },
            &mut |rows| {
                let mut rows: Vec<Vec<Value>> = rows
                    .into_iter()
                    .map(|r| {
                        let mut row = vec![Value::Null; width];
                        for (v, &p) in r.into_iter().zip(&wanted) {
                            row[p - node.at] = v;
                        }
                        row
                    })
                    .filter(|row| kept.as_ref().is_none_or(|k| k.admits(row)))
                    .collect();
                if let Some(p) = at {
                    let mut rel = Relation {
                        names: Vec::new(),
                        rows,
                        order: None,
                    };
                    pipeline::in_order(&mut rel, p)?;
                    rows = rel.rows;
                }
                if let Some((_, lead)) = &strand {
                    pipeline::in_place(&mut rows, lead)?;
                }
                let _flying = crate::room::flying(super::spill::rows_width(&rows));
                each(rows, order)
            },
        )?;
        self.accrued.add(&round);
        Ok(())
    }

    /// The question the question path reads: the select list, the FROM and as many of the
    /// conditions as it reads together; with none, where it reads them not.
    fn parsed(&self, select: &str, from: &str, conds: &[&Expr]) -> Result<Question> {
        parsed(self.reads.catalog(), select, from, conds)
    }

    /// A join whose every relation is a table, read as C1's round meets them where the question
    /// path reads the join; none where it does not.
    fn together(
        &mut self,
        node: &Node,
        preserved: bool,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<bool> {
        let mut leaves = Vec::new();
        node.leaves(&mut leaves);
        if leaves
            .iter()
            .any(|l| !matches!(l.what, What::Table { sample: None, .. }))
        {
            return Ok(false);
        }
        // Stopped for its room, the round has asked this already.
        if crate::room::current().is_some_and(|r| r.take_stop()) {
            return Ok(false);
        }
        let Some(from) = self.written(node, true)? else {
            return Ok(false);
        };
        let catalog = self.reads.catalog();
        let mut wanted: Vec<(usize, String, String)> = Vec::new();
        for l in &leaves {
            let What::Table { table, qualifier, .. } = &l.what else {
                unreachable!()
            };
            let t = &catalog.tables[*table];
            let names: Vec<String> = node_columns(t, l.width)
                .into_iter()
                .map(|c| t.column_at(c).name.clone())
                .collect();
            for p in l.at..l.at + l.width {
                if self.needed(p) {
                    wanted.push((p, qualifier.clone(), names[p - l.at].clone()));
                }
            }
        }
        if wanted.is_empty() {
            let l = leaves[0];
            let What::Table { table, qualifier, .. } = &l.what else {
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
            Err(_) => return Ok(false),
        };
        let read = self.reads.read_join(&q, &mut |rs| {
            let mut rows = Vec::with_capacity(rs.len());
            for r in rs {
                let mut row = vec![Value::Null; node.width];
                for (v, (p, _, _)) in r.into_iter().zip(&wanted) {
                    row[p - node.at] = v;
                }
                Self::merge(node, node.at, &mut row)?;
                rows.push(row);
            }
            let _flying = crate::room::flying(super::spill::rows_width(&rows));
            each(rows)
        });
        match read {
            Err(e) if super::join::past_the_room(&e) => {
                if let Some(r) = super::join::stopped_reads() {
                    self.accrued.add(&r);
                }
                if let Some(r) = crate::room::current() {
                    r.take_stop();
                }
                Ok(false)
            }
            read => {
                self.accrued.add(&read?);
                Ok(true)
            }
        }
    }

    /// The strand a FROM's first table is read along where it gives what the step wants, its pages
    /// fewer than the table's order of fewest with the sort it saves: its first relation a table, each
    /// join above it keeping its rows, and the strand holding what the step reads of it.
    fn choose_along(
        &self,
        first: &Node,
        wants: &super::strands::Wants,
    ) -> Result<Option<Along>> {
        let mut node = first;
        while let What::Join {
            left,
            right,
            kind,
            ..
        } = &node.what
        {
            if !matches!(kind, Kind::Inner | Kind::Left | Kind::Cross) || right.lateral() {
                return Ok(None);
            }
            node = left;
        }
        let What::Table { table, qualifier, .. } = &node.what else {
            return Ok(None);
        };
        let catalog = self.reads.catalog();
        let t = &catalog.tables[*table];
        let columns: Vec<usize> = node_columns(t, node.width);
        // Each expression a column of this table alone.
        let place = |e: &&Expr| -> Option<usize> {
            let (q, n) = pipeline::column_of(e)?;
            let found = pipeline::naming(
                self.from
                    .columns
                    .iter()
                    .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
                q.as_deref(),
                &n,
            );
            match found.as_slice() {
                [p] if (node.at..node.at + node.width).contains(p) => Some(*p - node.at),
                _ => None,
            }
        };
        let by: Option<Vec<usize>> = wants.by.iter().map(place).collect();
        let ordered: Option<Vec<usize>> = wants.ordered.iter().map(place).collect();
        let (Some(by), Some(ordered)) = (by, ordered) else {
            return Ok(None);
        };
        let at_columns = |ps: &[usize]| -> Vec<usize> { ps.iter().map(|&p| columns[p]).collect() };
        let needs: Vec<usize> = (node.at..node.at + node.width)
            .filter(|&p| self.needed(p))
            .map(|p| columns[p - node.at])
            .collect();
        // A strand holds no system column.
        let (by_at, ordered_at) = (at_columns(&by), at_columns(&ordered));
        if needs
            .iter()
            .chain(&by_at)
            .chain(&ordered_at)
            .any(|&c| crate::catalog::is_system(c))
        {
            return Ok(None);
        }
        let Some(o) = super::strands::led_by(t, &by_at, &ordered_at, &needs)
        else {
            return Ok(None);
        };
        let strand = t.orders[o].name.clone();
        let lead = by.len() + ordered.len();
        let at: Option<Vec<usize>> = t.orders[o].levels[..lead]
            .iter()
            .map(|l| columns.iter().position(|&c| crate::catalog::Level::Column(c) == *l))
            .collect();
        let Some(at) = at else {
            return Ok(None);
        };
        // Priced as a read of what the step reads of it, by its own conditions.
        let names: Vec<String> = needs.iter().map(|&c| t.columns[c].name.clone()).collect();
        let select = names
            .iter()
            .map(|n| format!("{}.{}", quote(qualifier), quote(n)))
            .collect::<Vec<_>>()
            .join(", ");
        if select.is_empty() {
            return Ok(None);
        }
        let from = format!("{} {}", quote_name(&t.name), quote(qualifier));
        let conds: Vec<&Expr> = match std::ptr::eq(first, node) {
            true => self.own_wheres(node),
            false => Vec::new(),
        };
        let q = self.parsed(&select, &from, &conds)?;
        if !self.reads.strand_wins(&q, &strand)? {
            return Ok(None);
        }
        Ok(Some(Along { node, strand, at }))
    }

    /// A join of tables as the question path reads it, each USING written as its ON: none where it
    /// writes no such join. `first` where the join is the first of its FROM item.
    fn written(&self, node: &Node, _first: bool) -> Result<Option<String>> {
        let catalog = self.reads.catalog();
        Ok(Some(match &node.what {
            What::Table { table, qualifier, .. } => format!(
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

    /// Before a LATERAL subquery is made for the rows before it: each set of values they give it,
    /// which its tables are kept to the keys of, where they fit the room.
    fn foresee_lateral(&self, node: &Node, rows: &Rows, prefix: &[Value]) -> Result<()> {
        let What::Lateral { at, .. } = &node.what else {
            return Ok(());
        };
        let kept = &self.laterals[*at].1;
        if kept.around.borrow().is_some() {
            return Ok(());
        }
        let around: &[Value] = self.outer.map_or(&[], |o| &o.values);
        kept.foresee(rows, &mut |r| {
            let mut set = prefix.to_vec();
            set.extend(r.iter().cloned());
            set.extend(around.iter().cloned());
            set
        })
    }

    /// A LATERAL subquery's rows for the row before it, its columns and the query's around the
    /// step read as the query around it, each of its tables read from the file once.
    fn laterally(
        &mut self,
        node: &Node,
        at: usize,
        before: Option<(usize, &[Value])>,
    ) -> Result<Rows> {
        let (start, row): (usize, &[Value]) = before.unwrap_or((node.at, &[]));
        let (q, kept) = &self.laterals[at];
        let mut columns: Vec<Col> = self.from.columns[start..start + row.len()]
            .iter()
            .map(marked)
            .collect();
        let mut values = row.to_vec();
        if let Some(o) = self.outer {
            columns.extend(o.columns.iter().map(marked));
            values.extend(o.values.iter().cloned());
        }
        let outer = Outer {
            columns,
            values,
            kept,
        };
        let cached = super::subqueries::Cached {
            reads: self.asked.reads,
            kept,
        };
        let asked = Asked {
            reads: &cached,
            answer_kept: true,
            ..self.asked
        };
        let (m, _) = made_by(
            asked,
            q,
            self.made,
            self.casings,
            Some(&outer),
            self.accrued,
        )?;
        m.into_rows()
    }

    /// A function's rows for the row before it, or for none where nothing comes before it.
    fn called(
        &mut self,
        node: &Node,
        call: &Call,
        spread: bool,
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
            subqueries: None,
        };
        let values = pipeline::call_rows(&call.expr, row, &frame)?;
        let fields = node.width - call.ordinality as usize;
        values
            .into_iter()
            .enumerate()
            .map(|(i, v)| {
                let mut out = match (fields, &v) {
                    (1, _) if !spread => vec![v],
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
                out.extend(by_place(found.star.clone()));
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
        subqueries: None,
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
    Ok(Made::of(
        Relation {
            names,
            rows: made,
            order: None,
        },
        types.iter().map(|&t| Some(t)).collect(),
        types.iter().map(|&t| carried_of(Some(t))).collect(),
    ))
}

/// A set operation's arms: each a step of its own, two met by their operator.
#[derive(Clone)]
enum Arm {
    /// A step, and what each of its columns is where an untyped literal or parameter makes it.
    Leaf { name: String, untyped: Vec<Untyped> },
    Op {
        op: SetOperator,
        all: bool,
        left: Box<Arm>,
        right: Box<Arm>,
    },
}

/// A column of an arm that takes the type its set operation finds: none, a NULL or a parameter, or
/// a quoted literal, read in that type.
#[derive(Clone, Debug)]
enum Untyped {
    No,
    Null,
    Literal(String),
}

/// Whether a question is a set operation.
pub(super) fn is_set_operation(q: &Query) -> bool {
    matches!(q.body.as_ref(), SetExpr::SetOperation { .. })
}

/// A query of a body alone.
fn query_of(body: SetExpr) -> Query {
    let parsed =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, "SELECT 1")
            .expect("a SELECT");
    let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() else {
        unreachable!("one SELECT")
    };
    let mut q = (**q).clone();
    q.body = Box::new(body);
    q
}

/// A set operation as steps: each arm a step of its own, the operation one more under `name`;
/// where it is sorted or limited, the operation under a name of its own, and `name` the step that
/// sorts and limits it.
fn lift_set(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    name: String,
    query: Query,
    renames: Vec<String>,
) -> Result<()> {
    if let Some(by) = &query.order_by {
        let sqlparser::ast::OrderByKind::Expressions(items) = &by.kind else {
            return Err(Error::new(format!("{by} is not read")));
        };
        for item in items {
            let named = match &item.expr {
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => true,
                Expr::Value(v) => matches!(v.value, SqlValue::Number(..)),
                _ => false,
            };
            if !named {
                return Err(Error::new(
                    "invalid UNION/INTERSECT/EXCEPT ORDER BY clause: only result column names \
                     can be used, not expressions or functions",
                ));
            }
        }
    }
    let arm = arm_of(catalog, out, visible, &query.body)?;
    let sorted = query.order_by.is_some() || query.limit_clause.is_some() || query.fetch.is_some();
    if !sorted {
        out.push(Step {
            name,
            query,
            renames,
            calls: Vec::new(),
            set: Some(arm),
            laterals: Vec::new(),
            recursion: None,
        });
        return Ok(());
    }
    let set = fresh(out, "set");
    out.push(Step {
        name: set.clone(),
        query: query.clone(),
        renames: Vec::new(),
        calls: Vec::new(),
        set: Some(arm),
        laterals: Vec::new(),
        recursion: None,
    });
    let parsed = sqlparser::parser::Parser::parse_sql(
        &sqlparser::dialect::PostgreSqlDialect {},
        "SELECT * FROM x",
    )?;
    let [sqlparser::ast::Statement::Query(star)] = parsed.as_slice() else {
        unreachable!("one SELECT")
    };
    let mut query = query;
    query.body = star.body.clone();
    if let SetExpr::Select(s) = query.body.as_mut() {
        s.from[0].relation = plain(&set, None);
    }
    out.push(Step {
        name,
        query,
        renames,
        calls: Vec::new(),
        set: None,
        laterals: Vec::new(),
        recursion: None,
    });
    Ok(())
}

/// A set operation's arm: an operation met of its own arms, or a step lifted out.
fn arm_of(
    catalog: &Catalog,
    out: &mut Vec<Step>,
    visible: &[String],
    body: &SetExpr,
) -> Result<Arm> {
    match body {
        SetExpr::SetOperation {
            left,
            op,
            set_quantifier,
            right,
        } => {
            let all = match set_quantifier {
                SetQuantifier::All => true,
                SetQuantifier::Distinct | SetQuantifier::None => false,
                q => return Err(Error::new(format!("{op} {q} is not read"))),
            };
            if matches!(op, SetOperator::Minus) {
                return Err(Error::new("MINUS is not read: write EXCEPT"));
            }
            let left = arm_of(catalog, out, visible, left)?;
            let right = arm_of(catalog, out, visible, right)?;
            Ok(Arm::Op {
                op: *op,
                all,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        SetExpr::Query(q)
            if q.with.is_none()
                && q.order_by.is_none()
                && q.limit_clause.is_none()
                && q.fetch.is_none()
                && matches!(q.body.as_ref(), SetExpr::SetOperation { .. }) =>
        {
            arm_of(catalog, out, visible, &q.body)
        }
        _ => {
            let query = match body {
                SetExpr::Query(q) => (**q).clone(),
                b => query_of(b.clone()),
            };
            let untyped = untyped_of(&query);
            let name = fresh(out, "arm");
            lift(catalog, out, visible, name.clone(), query, Vec::new())?;
            Ok(Arm::Leaf { name, untyped })
        }
    }
}

/// What each column of an arm's SELECT is where an untyped literal or parameter makes it, as
/// PostgreSQL leaves such an arm's output untyped; none where a star, a DISTINCT, a grouping or an
/// ORDER BY reads them.
fn untyped_of(q: &Query) -> Vec<Untyped> {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Vec::new();
    };
    let grouped =
        !matches!(&s.group_by, sqlparser::ast::GroupByExpr::Expressions(g, _) if g.is_empty());
    if q.order_by.is_some()
        || s.distinct.is_some()
        || grouped
        || s.projection.iter().any(|i| {
            matches!(
                i,
                SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..)
            )
        })
    {
        return Vec::new();
    }
    fn of(e: &Expr) -> Untyped {
        match e {
            Expr::Nested(e) => of(e),
            Expr::Value(v) => match &v.value {
                SqlValue::SingleQuotedString(s) => Untyped::Literal(s.clone()),
                SqlValue::Null | SqlValue::Placeholder(_) => Untyped::Null,
                _ => Untyped::No,
            },
            _ => Untyped::No,
        }
    }
    s.projection
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => of(e),
            _ => Untyped::No,
        })
        .collect()
}

/// An arm's rows met: its columns' names, each column's type and collation, and which are untyped.
struct Met {
    names: Vec<String>,
    types: Vec<Option<ColumnType>>,
    untyped: Vec<Untyped>,
    carried: Vec<Carried>,
    rows: Rows,
}

/// A set operation made: its arms met by their operators, each column typed as select_common_type
/// types its two arms' columns, the leftmost arm's names its own.
fn set_step(arm: &Arm, made: &Steps, casings: &Casings) -> Result<Made> {
    let m = met(arm, made, casings)?;
    let mut out = Made::of(
        Relation {
            names: m.names,
            rows: Vec::new(),
            order: None,
        },
        m.types,
        m.carried,
    );
    out.kept = Some(m.rows);
    Ok(out)
}

/// The values of `row` in the types `types`, each of another type or written as a literal assigned
/// to its column's.
fn in_types(
    row: &mut [Value],
    from: &[Option<ColumnType>],
    untyped: &[Untyped],
    types: &[ColumnType],
) -> Result<()> {
    for (c, &ty) in types.iter().enumerate() {
        let quoted = !matches!(untyped[c], Untyped::No);
        if from[c] == Some(ty) && !quoted {
            continue;
        }
        let v = std::mem::replace(&mut row[c], Value::Null);
        row[c] = match v.is_null() {
            true => v,
            false => coerce(v, ty, quoted)?,
        };
    }
    Ok(())
}

/// An arm's rows in the operation's types: as they lie where none changes.
fn arm_in_types(m: &Met, types: &[ColumnType]) -> Result<Rows> {
    let same = types.iter().enumerate().all(|(c, &ty)| {
        m.types[c] == Some(ty) && matches!(m.untyped[c], Untyped::No)
    });
    if same {
        return Ok(m.rows.clone());
    }
    let run = Run::new();
    m.rows.each_chunk(&mut |c| {
        for mut row in c {
            in_types(&mut row, &m.types, &m.untyped, types)?;
            run.push(row)?;
        }
        Ok(())
    })?;
    run.seal()?;
    Ok(Rows::one(run))
}

/// Each distinct row of two arms, by its first coming, as many times as the operator keeps it from
/// how many times each arm holds it. Past the room, the rows of the keys no row held has come for
/// are written, and met in a pass of their own, each key's rows whole in one pass.
fn image(
    op: SetOperator,
    all: bool,
    under: &[Option<crate::catalog::Casing>],
    mut sides: [Rows; 2],
) -> Result<Rows> {
    if !sides.iter().all(Rows::held_whole) {
        sides.iter().try_for_each(Rows::write_held)?;
    }
    let out = Run::new();
    out.last_reader();
    let room = crate::room::current();
    loop {
        // Each distinct row, by its first coming, with how many times each arm holds it.
        let mut at: HashMap<Vec<pipeline::Key>, usize> = HashMap::new();
        let mut groups: Vec<(Vec<Value>, usize, usize, u64)> = Vec::new();
        let mut bytes = 0u64;
        let mut closed = false;
        let past = [Run::past(), Run::past()];
        for (side, rows) in sides.iter().enumerate() {
            rows.each_chunk(&mut |c| {
                for row in c {
                    let k = pipeline::row_key(&row, under)?;
                    let i = match at.get(&k) {
                        Some(&i) => i,
                        None => {
                            let w = super::spill::row_width(&row);
                            let fits = !closed
                                && match &room {
                                    Some(r) => {
                                        r.hold(w)
                                            || (bytes + w <= CHUNK && {
                                                r.hold_anyway(w);
                                                true
                                            })
                                    }
                                    None => true,
                                };
                            if !fits {
                                closed = true;
                                past[side].push(row)?;
                                continue;
                            }
                            bytes += w;
                            at.insert(k, groups.len());
                            groups.push((row, 0, 0, w));
                            groups.len() - 1
                        }
                    };
                    match side {
                        0 => groups[i].1 += 1,
                        _ => groups[i].2 += 1,
                    }
                }
                Ok(())
            })?;
        }
        for (row, x, y, w) in groups {
            if let Some(r) = &room {
                r.let_go(w);
            }
            let times = match (op, all) {
                (SetOperator::Union, _) => 1,
                (SetOperator::Intersect, false) => usize::from(x > 0 && y > 0),
                (SetOperator::Intersect, true) => x.min(y),
                (_, false) => usize::from(x > 0 && y == 0),
                (_, true) => x.saturating_sub(y),
            };
            for _ in 1..times {
                out.push(row.clone())?;
            }
            if times > 0 {
                out.push(row)?;
            }
        }
        if past.iter().all(|p| p.len() == 0) {
            break;
        }
        let [a, b] = past;
        a.seal()?;
        b.seal()?;
        sides = [Rows::one(a), Rows::one(b)];
        // The pass to come holds its image in the room, those rows made before it written.
        out.write_held()?;
    }
    out.seal()?;
    Ok(Rows::one(out))
}

fn met(arm: &Arm, made: &Steps, casings: &Casings) -> Result<Met> {
    let (op, all, left, right) = match arm {
        Arm::Leaf { name, untyped } => {
            let m = made
                .get(name)
                .ok_or_else(|| Error::new("a set operation's arm was not made"))?;
            let n = m.rel.names.len();
            return Ok(Met {
                names: m.rel.names.clone(),
                types: m.types.clone(),
                untyped: (0..n)
                    .map(|c| untyped.get(c).cloned().unwrap_or(Untyped::No))
                    .collect(),
                carried: m.carried.clone(),
                rows: m.rows_of()?,
            });
        }
        Arm::Op {
            op,
            all,
            left,
            right,
        } => (*op, *all, left, right),
    };
    let context = match op {
        SetOperator::Union => "UNION",
        SetOperator::Intersect => "INTERSECT",
        _ => "EXCEPT",
    };
    let l = met(left, made, casings)?;
    let r = met(right, made, casings)?;
    let n = l.types.len();
    if r.types.len() != n {
        return Err(Error::new(format!(
            "each {context} query must have the same number of columns"
        )));
    }
    let concatenated = op == SetOperator::Union && all;
    let mut types = Vec::with_capacity(n);
    let mut carried = Vec::with_capacity(n);
    for c in 0..n {
        let side = |m: &Met| match m.untyped[c] {
            Untyped::No => m.types[c],
            _ => None,
        };
        let ty = typed::common_type(&[side(&l), side(&r)], context)?;
        // A literal is read in the type found before any row, as PostgreSQL's parser reads it.
        for m in [&l, &r] {
            if let Untyped::Literal(s) = &m.untyped[c] {
                coerce(Value::Text(s.clone()), ty, true)?;
            }
        }
        if !concatenated && !ty.grouped() {
            return Err(Error::new(format!(
                "could not identify an equality operator for type {}",
                ty.unequal_part().unwrap_or(ty).pg_name()
            )));
        }
        let k = carried_met(&l.carried[c], &r.carried[c], concatenated)?;
        carried.push(match k {
            Carried::None => carried_of(Some(ty)),
            k => k,
        });
        types.push(ty);
    }
    let lrows = arm_in_types(&l, &types)?;
    let rrows = arm_in_types(&r, &types)?;
    let rows = if concatenated {
        lrows.then(rrows)
    } else {
        let under = carried
            .iter()
            .map(|k| casings.carried_under(k))
            .collect::<Result<Vec<_>>>()?;
        image(op, all, &under, [lrows, rrows])?
    };
    Ok(Met {
        names: l.names,
        types: types.into_iter().map(Some).collect(),
        untyped: vec![Untyped::No; n],
        carried,
        rows,
    })
}

/// The collation a set operation's column carries, of its two arms' as PostgreSQL's
/// select_common_collation finds it: a collation other than the default over the default, two
/// others in conflict refused, or under UNION ALL left unsettled.
fn carried_met(a: &Carried, b: &Carried, none_ok: bool) -> Result<Carried> {
    let default = super::collate::DEFAULT;
    Ok(match (a, b) {
        (Carried::None, k) | (k, Carried::None) => k.clone(),
        (Carried::Unsettled, _) | (_, Carried::Unsettled) => Carried::Unsettled,
        (Carried::Named(x), Carried::Named(y)) if x == y || y == default => a.clone(),
        (Carried::Named(x), _) if x == default => b.clone(),
        _ if none_ok => Carried::Unsettled,
        (Carried::Named(x), Carried::Named(y)) => {
            return Err(Error::new(format!(
                "collation mismatch between implicit collations \"{x}\" and \"{y}\""
            )))
        }
    })
}

/// A question over several relations, answered: each step made in turn, its FROM met as one
/// relation, then read as the pipeline reads a step. One round, of every table read.
pub(super) fn answer(
    reads: &dyn Reads,
    q: &Query,
    params: &[String],
) -> Result<(RecordBatch, Round)> {
    let (last, held, accrued) = answered(reads, q, params, false)?;
    let batch = pipeline::batch_of(&last.rel)?;
    let mut round = accrued.round.into_inner();
    round.rows = batch.num_rows() as u64;
    round.rows_held = held as u64;
    if reads.snails() > 1 {
        round.snails = accrued.parts.into_inner();
    }
    Ok((batch, round))
}

/// The same, what it answers lying in the round's room, those rows past it written, for its caller to
/// take a chunk at a time: those rows, and their columns' names.
pub(super) fn answer_rows(
    reads: &dyn Reads,
    q: &Query,
    params: &[String],
) -> Result<(Rows, Vec<String>, Round)> {
    let (last, held, accrued) = answered(reads, q, params, true)?;
    let names = last.rel.names.clone();
    let rows = last.into_rows()?;
    let mut round = accrued.round.into_inner();
    round.rows = rows.len() as u64;
    round.rows_held = held as u64;
    if reads.snails() > 1 {
        round.snails = accrued.parts.into_inner();
    }
    Ok((rows, names, round))
}

fn answered(
    reads: &dyn Reads,
    q: &Query,
    params: &[String],
    answer_kept: bool,
) -> Result<(Made, usize, Accrued)> {
    let catalog = reads.catalog();
    let _rows = super::handed::rows_of(catalog);
    let asked = Asked {
        reads,
        params,
        release: catalog.release,
        now: crate::scalars::transaction_clock(),
        answer_kept,
    };
    let tables: Vec<&Table> = catalog.tables.iter().collect();
    let casings = Casings::of_tables(&tables, &catalog.collations);
    let accrued = Accrued {
        round: Default::default(),
        parts: std::cell::RefCell::new(vec![
            Round::default();
            if reads.snails() > 1 {
                reads.snails()
            } else {
                0
            }
        ]),
    };
    if let Some(r) = super::join::stopped_reads() {
        accrued.add(&r);
    }
    let _views = super::sysviews::holding();
    let before = catalog_steps(catalog, q)?;
    let (last, held) = made_by(asked, q, &before, &casings, None, &accrued)?;
    Ok((last, held, accrued))
}

/// What a question makes: each of its steps in turn, after the relations made before it, and its
/// last returned, with the rows they made in all. A subquery's is made so, the columns of the query
/// around it read after each of its steps' own.
pub(super) fn made_by(
    asked: Asked,
    q: &Query,
    before: &Steps,
    casings: &Casings,
    outer: Option<&Outer>,
    accrued: &Accrued,
) -> Result<(Made, usize)> {
    let catalog = asked.reads.catalog();
    let visible: Vec<String> = before.keys().cloned().collect();
    let mut steps = steps(catalog, q, &visible)?;
    let mut arms = HashMap::new();
    for s in &steps {
        if let Some(a) = &s.set {
            kept_arms(a, &mut arms);
        }
    }
    let mut making = Making {
        asked,
        made: before.clone(),
        casings: casings.clone(),
        outer,
        accrued,
        held: 0,
        arms,
    };
    // A recursion's own steps are made in each of its rounds, by the recursion.
    let theirs: Vec<usize> = steps
        .iter()
        .filter_map(|s| s.recursion.as_ref())
        .flat_map(|r| r.body.clone())
        .collect();
    // Each step's readers among the steps after it, a recursion reading what its own steps read:
    // a step is let go once the last of them is made.
    let body_of = |i: usize| -> Vec<usize> {
        steps[i]
            .recursion
            .as_ref()
            .map(|r| r.body.clone().collect())
            .unwrap_or_default()
    };
    let read: Vec<std::collections::HashSet<String>> = (0..steps.len())
        .map(|j| {
            std::iter::once(j)
                .chain(body_of(j))
                .flat_map(|k| step_words(&steps[k]))
                .collect()
        })
        .collect();
    let mut readers: HashMap<String, usize> = HashMap::new();
    for (i, s) in steps.iter().enumerate() {
        if s.name.is_empty() || theirs.contains(&i) || before.contains_key(&s.name) {
            continue;
        }
        let n = (i + 1..steps.len())
            .filter(|j| !theirs.contains(j) && read[*j].contains(&s.name))
            .count();
        if n > 0 {
            readers.insert(s.name.clone(), n);
        }
    }
    for i in 0..steps.len() {
        if theirs.contains(&i) {
            continue;
        }
        for (n, left) in &readers {
            if *left == 1 && read[i].contains(n) {
                if let Some(rows) = making.made.get(n).and_then(|m| m.kept.as_ref()) {
                    rows.last_reader();
                }
            }
        }
        if steps[i].recursion.is_some() {
            let (before, rest) = steps.split_at_mut(i);
            let m = recursive::make(&mut making, &rest[0], before)?;
            making.keep(&rest[0], m)?;
        } else {
            making.make(&mut steps[i])?;
        }
        making.read_by(&mut readers, &steps[i].name, &read[i]);
    }
    let last = making
        .made
        .remove("")
        .ok_or_else(|| Error::new("the question returned nothing"))?;
    let mut last = Rc::try_unwrap(last).unwrap_or_else(|m| (*m).clone());
    if !asked.answer_kept {
        if let Some(rows) = last.kept.take() {
            last.rel.rows = rows.all()?;
        }
    }
    Ok((last, making.held))
}

/// What making a statement's steps reads, and what it keeps: each relation made, by its name.
struct Making<'a, 'o> {
    asked: Asked<'a>,
    made: Steps,
    casings: Casings,
    outer: Option<&'o Outer<'o>>,
    accrued: &'o Accrued,
    held: usize,
    /// The second arm of each INTERSECT and EXCEPT of two steps, by its first: kept to its keys.
    arms: HashMap<String, String>,
}

/// The words a step's text names: its query's, its functions' calls', its LATERAL subqueries' and
/// its arms' steps.
fn step_words(step: &Step) -> Vec<String> {
    let mut text = step.query.to_string();
    for c in &step.calls {
        text.push(' ');
        text.push_str(&c.expr.to_string());
    }
    for (q, _) in &step.laterals {
        text.push(' ');
        text.push_str(&q.to_string());
    }
    let mut ws = words(&text);
    fn leaves(a: &Arm, out: &mut Vec<String>) {
        match a {
            Arm::Leaf { name, .. } => out.push(name.clone()),
            Arm::Op { left, right, .. } => {
                leaves(left, out);
                leaves(right, out);
            }
        }
    }
    if let Some(a) = &step.set {
        leaves(a, &mut ws);
    }
    if let Some(r) = &step.recursion {
        leaves(&r.right, &mut ws);
    }
    ws
}

/// Each INTERSECT and EXCEPT of two steps in an arm: its second step by its first.
fn kept_arms(arm: &Arm, out: &mut HashMap<String, String>) {
    if let Arm::Op {
        op, left, right, ..
    } = arm
    {
        if matches!(op, SetOperator::Intersect | SetOperator::Except) {
            if let (Arm::Leaf { name: l, .. }, Arm::Leaf { name: r, .. }) = (&**left, &**right) {
                out.insert(r.clone(), l.clone());
            }
        }
        kept_arms(left, out);
        kept_arms(right, out);
    }
}

impl Making<'_, '_> {
    /// One step made and kept.
    fn make(&mut self, step: &mut Step) -> Result<()> {
        let asked = self.asked;
        let m = match (&step.set, step.query.body.as_ref()) {
            (Some(arm), _) => set_step(arm, &self.made, &self.casings)?,
            (None, SetExpr::Values(_)) => {
                values_step(step, asked.params, asked.release, &self.casings)?
            }
            _ => {
                let before = self
                    .arms
                    .get(&step.name)
                    .and_then(|l| self.made.get(l))
                    .cloned();
                select_step(
                    asked,
                    step,
                    &self.made,
                    &mut self.casings,
                    self.outer,
                    self.accrued,
                    before.as_deref(),
                )?
            }
        };
        self.keep(step, m)
    }

    /// A step's relation kept under its name, its columns under the names its alias gives them.
    /// A step's relation kept under its name, its columns under the names its alias gives them.
    fn keep(&mut self, step: &Step, mut m: Made) -> Result<()> {
        if step.renames.len() > m.rel.names.len() {
            return Err(Error::new(format!(
                "WITH query \"{}\" has {} columns available but {} columns specified",
                apart::written(&step.name),
                m.rel.names.len(),
                step.renames.len()
            )));
        }
        for (n, r) in m.rel.names.iter_mut().zip(&step.renames) {
            *n = r.clone();
        }
        self.casings.renamed(&step.name, &step.renames);
        self.held += m.rel.rows.len() + m.kept.as_ref().map_or(0, Rows::len);
        // A step the steps after it read is kept in the round's room, the rows past it written; the
        // answer is the caller's.
        if !step.name.is_empty() && m.kept.is_none() {
            m.kept = Some(Rows::of(std::mem::take(&mut m.rel.rows))?);
        }
        self.made.insert(step.name.clone(), Rc::new(m));
        Ok(())
    }

    /// Lets go each step made here whose last reader is the step just made, `name`, which names
    /// `ws`.
    fn read_by(
        &mut self,
        readers: &mut HashMap<String, usize>,
        name: &str,
        ws: &std::collections::HashSet<String>,
    ) {
        let done: Vec<String> = readers
            .iter_mut()
            .filter(|(n, _)| n.as_str() != name && ws.contains(n.as_str()))
            .filter_map(|(n, left)| {
                *left -= 1;
                (*left == 0).then(|| n.clone())
            })
            .collect();
        for n in done {
            readers.remove(&n);
            self.made.remove(&n);
        }
    }
}

/// The =s of a subquery's WHERE that tie a column of its FROM to an expression of the query around
/// it alone, each as that column and that expression, in one type whose keys are its =.
fn ties<'e>(
    wheres: &[&'e Expr],
    from: &From,
    types: &typed::Types,
    casings: &Casings,
) -> Vec<(&'e Expr, &'e Expr)> {
    let around = |e: &Expr| -> Option<bool> {
        let mut read = Vec::new();
        pipeline::columns_read(e, &mut read);
        let mut sides = read.iter().map(|(q, n)| {
            match pipeline::naming(
                from.columns
                    .iter()
                    .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
                q.as_deref(),
                n,
            )
            .as_slice()
            {
                [i] => Some(*i >= from.outer_at),
                _ => None,
            }
        });
        let first = sides.next()??;
        sides.all(|s| s == Some(first)).then_some(first)
    };
    let mut out = Vec::new();
    for &c in wheres {
        let Expr::BinaryOp {
            left: a,
            op: BinaryOperator::Eq,
            right: b,
        } = c
        else {
            continue;
        };
        let same = match (
            types.get(&(&**a as *const Expr)),
            types.get(&(&**b as *const Expr)),
        ) {
            (Some(x), Some(y)) => x == y || (x.is_integer() && y.is_integer()),
            _ => false,
        };
        if !same || casings.compared_under_library(c) {
            continue;
        }
        let column = |e: &Expr| pipeline::column_of(e).is_some();
        match (around(a), around(b)) {
            (Some(false), Some(true)) if column(a) => out.push((&**a, &**b)),
            (Some(true), Some(false)) if column(b) => out.push((&**b, &**a)),
            _ => {}
        }
    }
    out
}

/// A step's FROM read, its rows handed to `each` a chunk at a time, as the round reads the tables in
/// it, with the column holding its first table's order where its rows keep it.
#[allow(clippy::too_many_arguments)]
fn read_from(
    asked: Asked,
    from: &From,
    named: &Named,
    wheres: Vec<&Expr>,
    casings: &Casings,
    types: &typed::Types,
    made: &Steps,
    accrued: &Accrued,
    laterals: &[(Query, super::subqueries::Kept)],
    outer: Option<&Outer>,
    kept: Vec<Keep>,
    rows_known: &std::cell::Cell<Option<u64>>,
    along: Option<(
        &super::strands::Wants,
        &std::cell::Cell<Option<super::strands::Led>>,
    )>,
    each: Each,
) -> Result<()> {
    let mut reading = Reading {
        reads: asked.reads,
        from,
        named,
        wheres,
        params: asked.params,
        release: asked.release,
        casings,
        types,
        now: asked.now,
        made,
        accrued,
        asked,
        laterals,
        outer,
        cached: HashMap::new(),
        asked_again: from.items.iter().any(|n| {
            let mut leaves = Vec::new();
            n.leaves(&mut leaves);
            leaves.iter().any(|l| l.lateral())
        }),
        order: None,
        kept,
        in_order: along.map(|a| a.1),
        along: None,
        rows_known,
        whole_from: None,
    };
    let (first, rest) = match from.items.split_first() {
        None => return each(vec![Vec::new()], None),
        Some(s) => s,
    };
    if let Some((wants, led)) = along {
        reading.along = reading.choose_along(first, wants)?;
        if reading.along.is_none() {
            led.set(None);
        }
    }
    if rest.is_empty() {
        reading.whole_from = Some(first);
        return reading.flow(first, None, true, each);
    }
    let mut rows = reading.place(first, None, true)?;
    let mut order = reading.order;
    for (k, item) in rest.iter().enumerate() {
        reading.order = None;
        if item.lateral() {
            reading.foresee_lateral(item, &rows, &[])?;
            let run = Run::new();
            rows.clone().each_chunk(&mut |c| {
                for r in c {
                    let xs = reading.place(item, Some((0, &r)), true)?;
                    xs.each_chunk(&mut |xc| {
                        for x in xc {
                            let mut row = r.clone();
                            row.extend(x);
                            run.push(row)?;
                        }
                        Ok(())
                    })?;
                }
                Ok(())
            })?;
            run.seal()?;
            if run.len() != rows.len() {
                order = None;
            }
            rows = Rows::one(run);
            continue;
        }
        // A table after the first, met by WHERE with the rows made before it, is kept to their keys.
        let k_item = match &item.what {
            What::Table { .. } => reading.keep_for_item(item, &rows)?,
            _ => None,
        };
        let pushed = k_item.is_some();
        reading.kept.extend(k_item);
        let xs = reading.place(item, None, true);
        if pushed {
            reading.kept.pop();
        }
        let xs = xs?;
        // The order holds where the first relation is read alone in it and the rest give one
        // row.
        if xs.len() != 1 {
            order = None;
        }
        let cross = |out: &mut dyn FnMut(Vec<Value>) -> Result<()>| -> Result<()> {
            rows.each_chunk(&mut |c| {
                xs.each_chunk(&mut |xc| {
                    for r in &c {
                        for x in &xc {
                            let mut row = r.clone();
                            row.extend(x.iter().cloned());
                            out(row)?;
                        }
                    }
                    Ok(())
                })
            })
        };
        // The last of them is handed on as it comes.
        if k + 1 == rest.len() {
            let mut hand = |c: Vec<Vec<Value>>| each(c, order);
            let mut chunk = Chunk::new(&mut hand);
            cross(&mut |row| chunk.push(row))?;
            return chunk.finish();
        }
        let run = Run::new();
        cross(&mut |row| run.push(row))?;
        run.seal()?;
        rows = Rows::one(run);
    }
    rows.each_chunk(&mut |c| each(c, order))
}

/// One SELECT of the statement, its FROM met as one relation, then read as the pipeline reads it;
/// a subquery's with the columns of the query around it after its FROM's.
fn select_step(
    asked: Asked,
    step: &mut Step,
    made: &Steps,
    casings: &mut Casings,
    outer: Option<&Outer>,
    accrued: &Accrued,
    arm_before: Option<&Made>,
) -> Result<Made> {
    let catalog = asked.reads.catalog();
    let (params, release, now) = (asked.params, asked.release, asked.now);
    // The FROM laid out, and its stars read as its columns.
    let mut from = {
        let SetExpr::Select(s) = step.query.body.as_ref() else {
            return Err(Error::new("a step is a SELECT"));
        };
        let systems = {
            let t = step.query.to_string().to_lowercase();
            t.contains("ctid") || t.contains("tableoid")
        };
        if systems {
            let ons = s.from.iter().flat_map(|f| &f.joins).filter_map(|j| {
                use sqlparser::ast::{JoinConstraint, JoinOperator as J};
                match &j.join_operator {
                    J::Inner(JoinConstraint::On(e))
                    | J::Join(JoinConstraint::On(e))
                    | J::Left(JoinConstraint::On(e))
                    | J::LeftOuter(JoinConstraint::On(e))
                    | J::Right(JoinConstraint::On(e))
                    | J::RightOuter(JoinConstraint::On(e))
                    | J::FullOuter(JoinConstraint::On(e)) => Some(e),
                    _ => None,
                }
            });
            for e in s.selection.iter().chain(s.having.iter()).chain(ons) {
                if let Some(x) = ctid_from_outside(e) {
                    return Err(Error::new(format!(
                        "{x}: a ctid from outside the statement is not read: a prune moves a live \
                         row to another block, where PostgreSQL's VACUUM never moves one"
                    )));
                }
            }
        }
        let mut laying = Laying {
            catalog,
            made,
            calls: &step.calls,
            laterals: &step.laterals,
            asked,
            casings,
            outer,
            release,
            columns: Vec::new(),
            systems,
            refused: HashMap::new(),
        };
        let mut items = Vec::with_capacity(s.from.len());
        for item in &s.from {
            items.push(laying.item(item)?);
        }
        From {
            columns: laying.columns,
            items,
            outer_at: 0,
            refused: laying.refused,
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
    from.outer_at = from.columns.len();
    if let Some(o) = outer {
        from.columns.extend(o.columns.iter().cloned());
    }
    let step = &*step;
    let (plans, seeded) =
        super::subqueries::Plans::of(asked, &step.query, &from.columns, made, casings)?;
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
                What::Table { table, qualifier, .. } => {
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
    // Each column of a table of the FROM of the domain its DDL gives it, for a row made of it.
    let mut domains: Vec<Option<String>> = vec![None; typed.len()];
    {
        let mut leaves = Vec::new();
        for n in &from.items {
            n.leaves(&mut leaves);
        }
        for l in leaves {
            match &l.what {
                What::Table { table, .. } => {
                    let t = &catalog.tables[*table];
                    for (k, c) in stored(t).enumerate() {
                        if let Some(d) = domains.get_mut(l.at + k) {
                            d.clone_from(&t.columns[c].domain);
                        }
                    }
                }
                What::Made { name, .. } => {
                    for (k, m) in super::handed::step_domains(name).into_iter().flatten().enumerate() {
                        if let Some(d) = domains.get_mut(l.at + k) {
                            *d = m;
                        }
                    }
                }
                _ => {}
            }
        }
    }
    let _domains = typed::domains_held(domains);
    let mut p = pipeline::prepare(
        &step.query,
        &typed,
        &keyed,
        release,
        now,
        &joined,
        seeded,
        plans.around(),
    )?;
    // A column it returns as it reads it, or cast to a domain, is of that domain for what reads it.
    if let SetExpr::Select(s) = step.query.body.as_ref() {
        let out = s
            .projection
            .iter()
            .map(|i| match i {
                SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                    typed::domain_in(e, &typed)
                }
                _ => None,
            })
            .collect();
        super::handed::step_made(&step.name, out);
    }
    let place = |q: &Option<String>, n: &str| {
        pipeline::naming(
            from.columns
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            q.as_deref(),
            n,
        )
    };
    // An aggregate of the query's around a subquery alone is that query's.
    if outer.is_some() {
        for a in p.aggregates() {
            let mut read = Vec::new();
            pipeline::columns_read(a, &mut read);
            let around = |(q, n): &(Option<String>, String)| {
                place(q, n).first().is_some_and(|&i| i >= from.outer_at)
            };
            if !read.is_empty() && read.iter().all(around) {
                return Err(Error::new(format!(
                    "{a} is not read: an aggregate of the query around a subquery"
                )));
            }
        }
    }
    // Each column it reads is one of its FROM's, refused before any row as PostgreSQL's parser
    // refuses one it finds none or two of. A parse tree handed to pg_get_expr is read there alone.
    let outside_trees: Vec<usize> = p
        .reads_but_trees()
        .iter()
        .filter_map(|(q, n)| match place(q, n).as_slice() {
            [one] => Some(*one),
            _ => None,
        })
        .collect();
    for (q, n) in p.reads() {
        let found = place(&q, &n);
        if let [one] = found.as_slice() {
            if let Some(why) = from.refused.get(one).filter(|_| outside_trees.contains(one)) {
                return Err(Error::new(format!(
                    "{why}, only a running server's, and a snail has no server: it is not read"
                )));
            }
        }
        match (found.len(), q) {
            (1, _) => {}
            (0, Some(q))
                if !from.columns.iter().any(|c| {
                    c.qualifier.as_deref().map(pipeline::unmarked) == Some(q.as_str())
                }) =>
            {
                return Err(from.no_entry(catalog, &q))
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
    let casings: &Casings = casings;
    // The FROM's rows.
    // The words it writes, its functions' calls and LATERAL subqueries among them.
    let text = std::iter::once(step.query.to_string())
        .chain(step.calls.iter().map(|c| c.expr.to_string()))
        .chain(step.laterals.iter().map(|(q, _)| q.to_string()))
        .collect::<Vec<_>>()
        .join(" ");
    let named = Named::of(&text);
    let mut wheres = Vec::new();
    if let SetExpr::Select(s) = step.query.body.as_ref() {
        if let Some(w) = &s.selection {
            conjuncts(w, &mut wheres);
        }
    }
    let tied = ties(&wheres, &from, &p.types, casings);
    // A conjunct reading no column, its subqueries reading nothing of the query around it, is read
    // once before any row, as PostgreSQL's one-time filter is: where it does not hold, the FROM gives
    // no rows.
    let mut shut = false;
    let gates: Vec<&Expr> = wheres
        .iter()
        .copied()
        .filter(|w| {
            let mut read = Vec::new();
            pipeline::columns_read(w, &mut read);
            read.is_empty() && plans.only_once(w)
        })
        .collect();
    if !gates.is_empty() {
        let subs =
            super::subqueries::Subs::new(&plans, asked, made, casings, &from.columns, accrued);
        let frame = pipeline::Frame {
            columns: from.frame_columns(0, from.columns.len()),
            params,
            release,
            casings,
            types: &p.types,
            now,
            subqueries: Some(&subs),
        };
        let row = vec![Value::Null; from.columns.len()];
        for g in gates {
            let v = pipeline::eval(g, &row, &frame, &pipeline::computed_none())?;
            if pipeline::truth(&v)? != Some(true) {
                shut = true;
                break;
            }
        }
    }
    // A conjunct naming the query around it is the pipeline's alone.
    let around: Vec<&str> = from.columns[from.outer_at..]
        .iter()
        .flat_map(|c| {
            std::iter::once(c.name.as_str()).chain(c.qualifier.as_deref().map(pipeline::unmarked))
        })
        .collect();
    wheres.retain(|w| {
        !words(&w.to_string())
            .iter()
            .any(|x| around.contains(&x.as_str()))
    });
    let mut leaves = Vec::new();
    for n in &from.items {
        n.leaves(&mut leaves);
    }
    let run_types = p.types.clone();
    let windowed = !p.windows().is_empty();
    let kept = outer.filter(|_| leaves.iter().all(|l| matches!(l.what, What::Table { .. })));
    let names: Vec<String> = from.columns.iter().map(|c| c.name.clone()).collect();
    let columns: Columns = from
        .columns
        .iter()
        .map(|c| (c.qualifier.clone(), c.name.clone()))
        .collect();
    let types = p.returns.clone();
    let carried = p.carried.clone();
    // A step the steps after it read keeps its rows in the round's room as it makes them, those past
    // it written; the answer is the caller's.
    let run_out = (!step.name.is_empty() || asked.answer_kept).then(super::spill::Run::new);
    let mut gathered: Vec<Vec<Value>> = Vec::new();
    let mut put = |rows: Vec<Vec<Value>>| -> Result<()> {
        match &run_out {
            Some(run) => rows.into_iter().try_for_each(|r| run.push(r)),
            None => {
                gathered.extend(rows);
                Ok(())
            }
        }
    };
    let subs = super::subqueries::Subs::new(&plans, asked, made, casings, &from.columns, accrued);
    let mut run = pipeline::StepRun::new(p);
    run.hands_on = true;
    if !plans.is_empty() {
        run.subqueries = Some(&subs);
    }
    if let Some(o) = outer {
        let mut row = vec![Value::Null; from.outer_at];
        row.extend(o.values.iter().cloned());
        run.empty_row = Some(row);
    }
    let fed = std::cell::Cell::new(false);
    let mut order_out = None;
    // A step of its own over one relation, or a join of relations, with no subquery asked after it,
    // may have its rows come in the order a strand of its first table gives.
    let wanted_along = match (&kept, arm_before, shut) {
        (None, None, false) if from.items.len() == 1 => super::strands::wants(&step.query),
        _ => None,
    };
    let in_order: std::cell::Cell<Option<super::strands::Led>> =
        std::cell::Cell::new(wanted_along.as_ref().map(|w| w.led));
    let along_wanted = wanted_along.as_ref().map(|w| (w, &in_order));
    let rows_known: std::cell::Cell<Option<u64>> = std::cell::Cell::new(None);
    let mut feed = |mut rows: Vec<Vec<Value>>, order: Option<usize>| -> Result<()> {
        if !fed.get() {
            if let Some(led) = in_order.get() {
                run.along(led);
            }
            if let Some(n) = rows_known.get() {
                run.expect_rows(n);
            }
        }
        fed.set(true);
        if let Some(o) = outer {
            for r in &mut rows {
                r.extend(o.values.iter().cloned());
            }
        }
        let rel = Relation {
            names: names.clone(),
            rows,
            order,
        };
        if let Some(out) = run.feed(rel, columns.clone(), params, release, casings)? {
            order_out = out.order;
            put(out.rows)?;
        }
        Ok(())
    };
    match kept {
        _ if shut => {}
        // A subquery's FROM of tables alone reads nothing of the query around it: read once, and
        // met with each row by the =s that tie them.
        Some(o) => {
            let found = o.kept.rows.borrow().get(&step.name).cloned();
            let tie = Tie {
                tied: &tied,
                from: &from,
                asked,
                casings,
                types: &run_types,
            };
            let parted = match found {
                Some(p) => p,
                None => {
                    let keeps = tied_keeps(&tied, &from, o, asked, casings, &run_types)?;
                    let in_passes = !tied.is_empty()
                        && o.kept.in_order.get()
                        && o.kept.around.borrow().is_some()
                        && o.kept.rows.borrow().is_empty();
                    let read = |each: Each| {
                        read_from(
                            asked,
                            &from,
                            &named,
                            wheres.clone(),
                            casings,
                            &run_types,
                            made,
                            accrued,
                            &step.laterals,
                            outer,
                            keeps.clone(),
                            &rows_known,
                            None,
                            each,
                        )
                    };
                    let parted = Rc::new(Parted::read(&tie, in_passes, &rows_known, read)?);
                    o.kept
                        .rows
                        .borrow_mut()
                        .insert(step.name.clone(), parted.clone());
                    // Past the room, the answers are made in the order of the keys' hashes: this
                    // row's among them.
                    if parted.in_passes() {
                        tie.in_hash_order(o)?;
                        o.kept.answers_now.set(true);
                        feed(Vec::new(), None)?;
                        return Ok(Made::of(
                            Relation {
                                names: Vec::new(),
                                rows: Vec::new(),
                                order: None,
                            },
                            types,
                            carried,
                        ));
                    }
                    parted
                }
            };
            parted.rows_for(&tie, o, &mut feed)?;
        }
        None => {
            let keeps = match arm_before {
                Some(m)
                    if !windowed
                        && step.query.limit_clause.is_none()
                        && step.query.fetch.is_none() =>
                {
                    arm_keeps(&step.query, &from, m)?
                }
                _ => Vec::new(),
            };
            // A step holding subqueries has every row of its FROM before the first asks, and reads
            // no table while it reads its rows: any other is handed them as they come.
            match plans.is_empty() {
                true => read_from(
                    asked,
                    &from,
                    &named,
                    wheres,
                    casings,
                    &run_types,
                    made,
                    accrued,
                    &step.laterals,
                    outer,
                    keeps,
                    &rows_known,
                    along_wanted,
                    &mut feed,
                )?,
                false => {
                    let place = super::spill::Run::new();
                    let mut order = None;
                    read_from(
                        asked,
                        &from,
                        &named,
                        wheres,
                        casings,
                        &run_types,
                        made,
                        accrued,
                        &step.laterals,
                        outer,
                        keeps,
                        &rows_known,
                        along_wanted,
                        &mut |c, at| {
                            order = at;
                            c.into_iter().try_for_each(|r| place.push(r))
                        },
                    )?;
                    place.seal()?;
                    let place = super::spill::Rows::one(place);
                    rows_known.set(Some(place.len() as u64));
                    subs.foresee(&place)?;
                    place.each_chunk(&mut |c| feed(c, order))?;
                }
            }
        }
    }
    if !fed.get() {
        feed(Vec::new(), None)?;
    }
    let (names_out, tail_order) = run.finish_each(params, release, casings, &mut put)?;
    order_out = order_out.or(tail_order);
    let rel = Relation {
        names: names_out,
        rows: gathered,
        order: order_out,
    };
    let mut m = Made::of(rel, types, carried);
    if let Some(run) = run_out {
        run.seal()?;
        m.kept = Some(super::spill::Rows::one(run));
    }
    Ok(m)
}

/// What the rows that ask for a subquery hold of the columns its =s tie to the query around it:
/// each set of values they give what it names, the tie's expression read on it.
fn tied_keeps(
    tied: &[(&Expr, &Expr)],
    from: &From,
    o: &Outer,
    asked: Asked,
    casings: &Casings,
    types: &typed::Types,
) -> Result<Vec<Keep>> {
    let Some(sets) = o.kept.around.borrow().clone() else {
        return Ok(Vec::new());
    };
    let mut places = Vec::new();
    let mut exprs = Vec::new();
    for (a, b) in tied {
        let Some((q, n)) = pipeline::column_of(a) else {
            continue;
        };
        let found = pipeline::naming(
            from.columns[..from.outer_at]
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            q.as_deref(),
            &n,
        );
        if let [i] = found.as_slice() {
            places.push(*i);
            exprs.push(*b);
        }
    }
    if places.is_empty() {
        return Ok(Vec::new());
    }
    let frame = pipeline::Frame {
        columns: from.frame_columns(0, from.columns.len()),
        params: asked.params,
        release: asked.release,
        casings,
        types,
        now: asked.now,
        subqueries: None,
    };
    let none = pipeline::computed_none();
    let set_rows = Rows::of(sets)?;
    Ok(Keep::of_rows(places, &set_rows, false, &mut |set| {
        let mut row = vec![Value::Null; from.outer_at];
        row.extend(set.iter().cloned());
        exprs
            .iter()
            .map(|e| pipeline::eval(e, &row, &frame, &none))
            .collect()
    })?
    .into_iter()
    .collect())
}

/// What a set operation's first arm holds of the columns of the table its second arm, a SELECT of
/// one table, returns as its own, by their places among the arm's columns. NULLs meet, as the
/// operation meets its arms' rows.
fn arm_keeps(q: &Query, from: &From, before: &Made) -> Result<Vec<Keep>> {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Ok(Vec::new());
    };
    if from.items.len() != 1 || !matches!(from.items[0].what, What::Table { .. }) {
        return Ok(Vec::new());
    }
    let mut pairs = Vec::new();
    for (i, item) in s.projection.iter().enumerate() {
        let e = match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
            _ => return Ok(Vec::new()),
        };
        let Some((q, n)) = pipeline::column_of(e) else {
            continue;
        };
        let found = pipeline::naming(
            from.columns[..from.outer_at]
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            q.as_deref(),
            &n,
        );
        if let [p] = found.as_slice() {
            pairs.push((*p, i));
        }
    }
    if pairs.is_empty() || before.rel.names.len() != s.projection.len() {
        return Ok(Vec::new());
    }
    Ok(Keep::of_rows(
        pairs.iter().map(|p| p.0).collect(),
        &before.rows_of()?,
        true,
        &mut |r| Ok(pairs.iter().map(|p| r[p.1].clone()).collect()),
    )?
    .into_iter()
    .collect())
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
            return true;
        }
        for cte in &with.cte_tables {
            // A step's own WITH makes steps of its own.
            if cte.query.with.is_some() {
                return true;
            }
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
pub(super) struct Empty<'c>(pub(super) &'c Catalog);

impl Reads for Empty<'_> {
    fn catalog(&self) -> &Catalog {
        self.0
    }

    fn read_each(
        &self,
        _q: &Question,
        _order: Option<&str>,
        _kept: Option<&KeySet>,
        _most: &std::cell::Cell<Option<u64>>,
        _each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        Ok(Round::default())
    }

    fn strand_wins(&self, _q: &Question, _strand: &str) -> Result<bool> {
        Ok(false)
    }

    fn read_join(
        &self,
        _q: &Question,
        _each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        Ok(Round::default())
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

    fn read_join(
        &self,
        q: &Question,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        let at: Vec<(usize, usize)> = q.sides.iter().map(|s| self.at[s.table]).collect();
        super::join::read_join(&self.snails, &at, q, each)
    }

    fn strand_wins(&self, q: &Question, strand: &str) -> Result<bool> {
        let (s, t) = self.at[q.sides[0].table];
        let mut local = q.clone();
        local.sides[0].table = t;
        Reads::strand_wins(self.snails[s], &local, strand)
    }

    fn read_each(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
        most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        let (s, t) = self.at[q.sides[0].table];
        let mut local = q.clone();
        local.sides[0].table = t;
        let mut r = self.snails[s].read_blocks(&local, order, kept, most, each)?;
        let mut parts = vec![Round::default(); self.snails.len()];
        parts[s] = r.clone();
        r.snails = parts;
        Ok(r)
    }

    fn snails(&self) -> usize {
        self.snails.len()
    }
}

/// The ON a join meets its relations by, where it has one.
pub(super) fn join_on(j: &JoinOperator) -> Option<&Expr> {
    match j {
        JoinOperator::Join(JoinConstraint::On(e))
        | JoinOperator::Inner(JoinConstraint::On(e))
        | JoinOperator::Left(JoinConstraint::On(e))
        | JoinOperator::LeftOuter(JoinConstraint::On(e))
        | JoinOperator::Right(JoinConstraint::On(e))
        | JoinOperator::RightOuter(JoinConstraint::On(e))
        | JoinOperator::FullOuter(JoinConstraint::On(e)) => Some(e),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::Named;
    use crate::read::pipeline::{MERGED, OUTER};

    #[test]
    fn a_name_after_a_relation_s_name_is_that_relation_s_alone() {
        let n = Named::of("SELECT a.id, b . /* a comment */ v, s.c.w FROM a");
        assert!(n.names(Some("a"), "id"));
        assert!(!n.names(Some("b"), "id"));
        assert!(!n.names(None, "id"));
        assert!(n.names(Some("b"), "v"));
        assert!(!n.names(Some("a"), "v"));
        // A schema's name stands before a relation's.
        assert!(n.names(Some("c"), "w"));
        assert!(!n.names(Some("s"), "w"));
        // Its mark is no part of the name it goes by.
        assert!(n.names(Some(&format!("{MERGED}a")), "id"));
        assert!(n.names(Some(&format!("{OUTER}a")), "id"));
    }

    #[test]
    fn a_name_alone_is_every_relation_s_and_a_label_or_a_qualifier_is_none() {
        let n = Named::of("SELECT id, x AS y, CAST(z AS t), q.r, (u).f, \"as\" FROM a AS d");
        assert!(n.names(Some("a"), "id"));
        assert!(n.names(Some("b"), "id"));
        assert!(n.names(None, "id"));
        assert!(n.names(Some("b"), "x"));
        assert!(!n.names(Some("b"), "y"));
        assert!(!n.names(Some("b"), "d"));
        assert!(!n.names(Some("b"), "t"));
        assert!(!n.names(Some("b"), "q"));
        // What follows anything but a relation's name counts as a name alone.
        assert!(n.names(Some("b"), "f"));
        assert!(n.names(Some("b"), "u"));
        // AS only where it is the keyword.
        assert!(n.names(Some("b"), "as"));
        let n = Named::of("SELECT \"as\" y");
        assert!(n.names(Some("b"), "y"));
    }

    #[test]
    fn a_name_is_folded_where_it_is_bare_and_kept_where_it_is_quoted() {
        let n = Named::of("SELECT A.ID, \"B\".\"V\", c.\"W\", D FROM t");
        assert!(n.names(Some("a"), "id"));
        assert!(!n.names(Some("A"), "id"));
        assert!(n.names(Some("B"), "V"));
        assert!(!n.names(Some("b"), "V"));
        assert!(!n.names(Some("B"), "v"));
        assert!(n.names(Some("c"), "W"));
        assert!(!n.names(Some("c"), "w"));
        assert!(n.names(Some("x"), "d"));
        assert!(!n.names(Some("x"), "D"));
    }

    #[test]
    fn a_star_after_a_relation_s_name_is_every_column_of_it_and_any_other_star_none() {
        let n = Named::of("SELECT a.*, count(*), 2 * x.y FROM t");
        assert!(n.names(Some("a"), "anything"));
        assert!(!n.names(Some("b"), "anything"));
        assert!(!n.names(None, "anything"));
    }

    #[test]
    fn a_text_the_tokenizer_does_not_read_may_mean_any_column() {
        let n = Named::of("SELECT 'open");
        assert!(n.names(Some("a"), "id"));
        assert!(n.names(None, "id"));
    }
}
