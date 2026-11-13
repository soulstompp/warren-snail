//! A step's subqueries: a scalar subquery, EXISTS, and IN, ANY or ALL over a subquery, in its select
//! list, WHERE, HAVING, GROUP BY, ORDER BY or a join's ON. One that reads nothing of the query
//! around it is a step run once, the first time a row asks for it. One that reads that query is
//! answered for each set of values a row gives what it names, its tables read from the file once,
//! its FROM's rows kept where they read nothing of the query around it, and met with each row by the
//! =s of its WHERE that tie a column of its own to that query; where none ties them, every kept row
//! is read for each. An ON's are answered for each pair of rows its join meets.

use super::pipeline::{self, Casings, Computed, Frame, Key, Value};
use super::question::Question;
use super::relations::{self, Accrued, Asked, Col, Made, Outer, Reads, Steps};
use super::held::{Columns, KeyRows, KeySet};
use super::spill::{Rows, Run};
use super::typed::Types;
use super::Round;
use crate::catalog::{Catalog, ColumnType};
use crate::{Error, Result};
use sqlparser::ast::{
    BinaryOperator, Expr, GroupByExpr, OrderByKind, Query, SelectItem, SetExpr, TableFactor,
};
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

/// What a subquery's reads keep between the rows it is answered for: each question the question
/// path read, and each of its steps' FROM rows where they read nothing of the query around it.
#[derive(Default)]
pub(super) struct Kept {
    pub(super) rows: RefCell<HashMap<String, Rc<relations::Parted>>>,
    /// Each set of values the rows that may ask for it give what it names, where they are known
    /// before the first asks: its tables are kept to their keys.
    pub(super) around: RefCell<Option<super::held::Columns>>,
    around_held: RefCell<Option<crate::room::Holding>>,
    /// Each question's rows, kept in the round's room as they are read.
    places: RefCell<HashMap<String, Rows>>,
    /// Whether its answers may be made in the order of a hash of what ties it to the query around
    /// it: it is one step, answered for each set of values once.
    pub(super) in_order: std::cell::Cell<bool>,
    /// Whether a kept table found past the room asks for that: its rows of the keys that hash lowest
    /// are held, so the answers are made now, lowest first.
    pub(super) answers_now: std::cell::Cell<bool>,
}

/// The snail's tables read through what a subquery keeps: each question from the file once, then
/// as it was read.
pub(super) struct Cached<'a> {
    pub(super) reads: &'a dyn Reads,
    pub(super) kept: &'a Kept,
}

impl Reads for Cached<'_> {
    fn catalog(&self) -> &Catalog {
        self.reads.catalog()
    }

    fn catalog_rows(&self) -> &Catalog {
        self.reads.catalog_rows()
    }

    fn read_join(
        &self,
        q: &Question,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        let key = format!("{q:?}");
        let found = self.kept.places.borrow().get(&key).cloned();
        if let Some(rows) = found {
            rows.each_chunk(each)?;
            return Ok(Round::default());
        }
        let run = Run::new();
        let round = self.reads.read_join(q, &mut |rows| {
            for r in &rows {
                run.push(r.clone())?;
            }
            each(rows)
        })?;
        run.seal()?;
        self.kept.places.borrow_mut().insert(key, Rows::one(run));
        Ok(round)
    }

    fn strand_wins(&self, q: &Question, strand: &str) -> Result<bool> {
        self.reads.strand_wins(q, strand)
    }

    fn read_each(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&super::walk::KeySet>,
        most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        if kept.is_some() || order.is_some() {
            return self.reads.read_each(q, order, kept, most, each);
        }
        let key = format!("{q:?}");
        let found = self.kept.places.borrow().get(&key).cloned();
        if let Some(rows) = found {
            most.set(Some(rows.len() as u64));
            rows.each_chunk(each)?;
            return Ok(Round::default());
        }
        let run = Run::new();
        let round = self.reads.read_each(q, None, None, most, &mut |rows| {
            let mut copy = Vec::with_capacity(rows.len());
            for r in &rows {
                copy.push(r.clone());
            }
            copy.into_iter().try_for_each(|r| run.push(r))?;
            each(rows)
        })?;
        run.seal()?;
        self.kept.places.borrow_mut().insert(key, Rows::one(run));
        Ok(round)
    }

    fn snails(&self) -> usize {
        self.reads.snails()
    }

    fn holds_rows(&self) -> bool {
        self.reads.holds_rows()
    }
}

/// What a subquery's place asks of its rows.
enum Asks<'q> {
    /// A scalar subquery: its one row's one value, NULL where it returns none.
    Scalar,
    Exists {
        negated: bool,
    },
    /// `x op ANY (…)`, IN as `= ANY (…)`, and `x op ALL (…)`; NOT IN is `NOT (x = ANY (…))`.
    Compared {
        left: &'q Expr,
        op: BinaryOperator,
        any: bool,
        negated: bool,
    },
}

/// The rows a subquery returned: those its place reads, held; past the room, in a place of their own,
/// read a chunk at a time by each row that asks; and the keys of its one column, where an = compares
/// by them and they fit the room.
struct Returned {
    rows: Vec<Vec<Value>>,
    past: Option<Rows>,
    keys: OnceCell<Option<(KeySet, bool, crate::room::Holding)>>,
    /// What they hold in the round's room.
    _held: crate::room::Holding,
}

impl Returned {
    fn of(asks: &Asks, m: Made) -> Result<Rc<Returned>> {
        let rows = m.into_rows()?;
        let most = match asks {
            Asks::Exists { .. } => Some(1),
            Asks::Scalar => Some(2),
            Asks::Compared { .. } => None,
        };
        let (held, past) = match most {
            Some(n) => (rows.first(n)?, None),
            None if rows.held_whole() => (rows.all()?, None),
            None => (Vec::new(), Some(rows)),
        };
        Ok(Rc::new(Returned {
            _held: crate::room::holding(super::spill::rows_width(&held)),
            rows: held,
            past,
            keys: OnceCell::new(),
        }))
    }

    fn of_rows(rows: Vec<Vec<Value>>) -> Rc<Returned> {
        Rc::new(Returned {
            _held: crate::room::holding(super::spill::rows_width(&rows)),
            rows,
            past: None,
            keys: OnceCell::new(),
        })
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty() && self.past.is_none()
    }

    /// The keys of its one column, and whether a NULL lies among them; none where they do not fit
    /// the room.
    fn keys(&self) -> Option<(&KeySet, bool)> {
        let built = self.keys.get_or_init(|| {
            let mut held = crate::room::holding(0);
            let mut keys = KeySet::default();
            let mut nulls = false;
            let mut add = |c: &[Vec<Value>]| -> bool {
                for r in c {
                    match r[0].is_null() {
                        true => nulls = true,
                        false => {
                            if keys.ask(&[pipeline::key(&r[0])]).is_some() {
                                continue;
                            }
                            if !held.fits(keys.width()) {
                                return false;
                            }
                            keys.hold();
                        }
                    }
                }
                true
            };
            let fits = match &self.past {
                None => {
                    // Held already: its keys are counted beside its rows.
                    add(&self.rows)
                }
                Some(rows) => {
                    let mut fits = true;
                    let read = rows.each_chunk(&mut |c| {
                        fits = fits && add(&c);
                        Ok(())
                    });
                    fits && read.is_ok()
                }
            };
            fits.then_some((keys, nulls, held))
        });
        built.as_ref().map(|(k, n, _)| (k, *n))
    }

    /// `one` read on its rows, chunk by chunk past the room: as `any` or all of its rows comparing so
    /// would be.
    fn compared(
        &self,
        any: bool,
        one: &mut dyn FnMut(&[Vec<Value>]) -> Result<Value>,
    ) -> Result<Value> {
        let Some(rows) = &self.past else {
            return one(&self.rows);
        };
        let mut unknown = false;
        let mut decided = None;
        rows.each_chunk(&mut |c| {
            if decided.is_none() {
                match one(&c)? {
                    Value::Bool(b) if b == any => decided = Some(b),
                    Value::Bool(_) => {}
                    _ => unknown = true,
                }
            }
            Ok(())
        })?;
        Ok(match decided {
            Some(b) => Value::Bool(b),
            None if unknown => Value::Null,
            None => Value::Bool(!any),
        })
    }
}

/// Answers past the room, a subquery's for each set of values, where its kept table's rows are
/// gone by the time a row asks: written once, and read back for each row that asks.
#[derive(Default)]
struct Answers {
    run: Option<Run>,
    at: HashMap<String, (u64, u32)>,
    _held: Option<crate::room::Holding>,
}

/// A subquery's last step read off its text once a round and, where it is an EXISTS read as the
/// join it is, how it meets the query around it and what it met.
struct Readied<'a> {
    ready: Box<relations::Ready<'a>>,
    joined: Option<relations::Joined<'a>>,
    met: RefCell<Option<Met>>,
}

/// An EXISTS read as the join it is, made once a round: the values its subquery's rows give its
/// ties, by their keys, held in the round's room, each with its rows where a pair is met by more.
/// Where they pass the room before any row asks, the truth of each row of the step found ahead,
/// both sides cut on their keys' hash, by its place among them; those of the rows being read held
/// by their keys.
#[derive(Default)]
struct Met {
    keys: KeySet,
    rows: KeyRows,
    pairs: Columns,
    held: Option<crate::room::Holding>,
    /// Whether they passed the room: a row whose truth is not held is answered as a subquery's.
    past: bool,
    ahead: Option<(Vec<u64>, crate::room::Holding)>,
    reading: HashMap<Vec<Key>, bool>,
}

/// An EXISTS whose rows passed the room before any row asked: the rows it held, its parts, the
/// parts of the step's rows asking beside them, and each row's truth found so far.
struct Ahead<'a> {
    places: &'a [usize],
    readied: Rc<Readied<'a>>,
    met: Met,
    theirs: super::spill::Parts,
    asking: super::spill::Parts,
    bits: Vec<u64>,
    bits_held: crate::room::Holding,
}

/// How many times a part of an EXISTS's rows past the room is cut again before it is held whole, as
/// a join's batch is.
const CUTS: u32 = 8;

/// How many parts an EXISTS's rows past the room are cut into where how many there are is not
/// known.
const PARTS_UNKNOWN: u64 = 16;

impl Met {
    /// The bytes a row of the subquery's more adds held.
    fn width(&self, key: &[Key], row: &[Value], by_pairs: bool) -> u64 {
        match by_pairs {
            true => self.rows.width_of(key) + self.pairs.width_of(row),
            false => self.keys.width_of(key),
        }
    }

    /// A row of the subquery's more, by the values it gives its ties.
    fn hold(&mut self, key: Vec<Key>, row: Vec<Value>, by_pairs: bool) {
        match by_pairs {
            true => {
                self.rows.push(&key, self.pairs.len() as u32);
                self.pairs.push(row);
            }
            false => {
                self.keys.insert(&key);
            }
        }
    }

    /// Whether a row it holds meets the values a row asking gives, by the keys of its ties and by
    /// `pair` where a pair is met by more.
    fn meets(
        &self,
        key: &[Key],
        by_pairs: bool,
        pair: &mut dyn FnMut(&[Value]) -> Result<bool>,
    ) -> Result<bool> {
        if !by_pairs {
            return Ok(self.keys.contains(key));
        }
        for i in self.rows.rows(key) {
            if pair(&self.pairs.row(i as usize))? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// The keys of values.
fn keys_of(values: &[Value]) -> Vec<Key> {
    values.iter().map(pipeline::key).collect()
}

/// What a row asking an EXISTS read as a join is told its truth by while it is read: the keys of
/// its ties' values, and of every value it gives where a pair is met by more.
fn reading_key(tie: &[Key], values: &[Value], by_pairs: bool) -> Vec<Key> {
    let mut k = tie.to_vec();
    if by_pairs {
        k.extend(values.iter().map(pipeline::key));
    }
    k
}

/// The value at a place of a step's columns of one of its FROM's rows, those of the query around it
/// after them.
fn value_at(row: &[Value], at: usize, around: &[Value]) -> Value {
    match row.get(at) {
        Some(v) => v.clone(),
        None => around.get(at - row.len()).cloned().unwrap_or(Value::Null),
    }
}

/// A subquery of a step, as its place asks it.
struct Plan<'q> {
    /// Its query as written, or with each aggregate of the query around it it calls read as a column
    /// that query hands it.
    query: std::borrow::Cow<'q, Query>,
    /// Those aggregates, by their places in the query around it, which computes them on each of its
    /// groups; and the columns they are handed as, in their order.
    lifted: Vec<*const Expr>,
    lifted_columns: Vec<Col>,
    asks: Asks<'q>,
    /// The type of its first column.
    first: Option<ColumnType>,
    /// Where it reads the query around it: the places of that query's columns its words name.
    around: Option<Vec<usize>>,
    /// Of those, the ones it cannot be read without, where it is read on each group or stands in a
    /// FULL JOIN's ON.
    reads_around: Vec<usize>,
    /// What it returned: once, where it reads nothing of the query around it; else for each set of
    /// values of what it names.
    once: RefCell<Option<Rc<Returned>>>,
    by_values: RefCell<HashMap<String, Rc<Returned>>>,
    /// Those past the room, and each refused for a set of values, said when a row asks.
    written: RefCell<Answers>,
    refused: RefCell<HashMap<String, String>>,
    kept: Kept,
    /// Its statement read once a round, where it is one SELECT after steps reading nothing of the
    /// query around it; and the columns of that query as it reads them.
    read: OnceCell<Option<relations::Once>>,
    around_columns: OnceCell<Vec<Col>>,
    /// Whether it may be read as the join it is: an EXISTS reading the query around it, one SELECT
    /// handed no aggregate of that query.
    joinable: bool,
}

/// A step's subqueries, by their places in its query.
#[derive(Default)]
pub(super) struct Plans<'q> {
    plans: HashMap<*const Expr, Plan<'q>>,
    around: Vec<usize>,
}

impl<'q> Plans<'q> {
    pub(super) fn is_empty(&self) -> bool {
        self.plans.is_empty()
    }

    /// Each subquery a step's query holds outside any other, read over tables holding no rows before
    /// any row, as PostgreSQL's parser reads it: refused where it returns columns its place does not
    /// take, and read as reading the query around it where it names a column only that query has.
    /// With the type each scalar subquery takes.
    pub(super) fn of(
        asked: Asked,
        q: &'q Query,
        columns: &[Col],
        made: &Steps,
        casings: &Casings,
    ) -> Result<(Plans<'q>, Types)> {
        // Those read on each group where the step groups, and those read before it groups, each
        // with the clause it stands in where an aggregate of the step's may not: WHERE, GROUP BY.
        let mut after = Vec::new();
        let mut before = Vec::new();
        let mut clauses: Vec<Option<&'static str>> = Vec::new();
        if let SetExpr::Select(s) = q.body.as_ref() {
            for item in &s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item
                {
                    on_groups(e, &mut after, &mut before);
                }
            }
            if let Some(e) = &s.having {
                on_groups(e, &mut after, &mut before);
            }
            clauses.resize(before.len(), None);
            if let Some(e) = &s.selection {
                pipeline::subqueries_in(e, &mut before);
            }
            clauses.resize(before.len(), Some("WHERE"));
            if let GroupByExpr::Expressions(g, _) = &s.group_by {
                for e in g {
                    pipeline::subqueries_in(e, &mut before);
                }
            }
            clauses.resize(before.len(), Some("GROUP BY"));
        }
        if let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) {
            for o in os {
                on_groups(&o.expr, &mut after, &mut before);
            }
        }
        clauses.resize(before.len(), None);
        let mut plans = Plans::default();
        let mut seeded = Types::new();
        let found = after
            .into_iter()
            .map(|e| (e, true, None))
            .chain(before.into_iter().zip(clauses).map(|(e, c)| (e, false, c)));
        for (e, grouped, clause) in found {
            let plan = Plan::of(asked, e, columns, made, casings, grouped, clause, None)?;
            if let (Asks::Scalar, Some(t)) = (&plan.asks, plan.first) {
                seeded.insert(e as *const Expr, t);
            }
            plans.around.extend(plan.reads_around.iter().copied());
            plans.plans.insert(e as *const Expr, plan);
        }
        Ok((plans, seeded))
    }

    /// Each subquery a join's ON holds outside any other, read as a WHERE's are, over `columns`: the
    /// join's two sides', then the query's around the step. Its refusals name the ON, and the
    /// relations `unseen` of the FROM it cannot reference. Where `sided`, each is read again with one
    /// column hidden at a time, so the columns it cannot be read without are known.
    pub(super) fn of_on(
        asked: Asked,
        on: &'q Expr,
        columns: &[Col],
        made: &Steps,
        casings: &Casings,
        sided: bool,
        unseen: &[String],
    ) -> Result<(Plans<'q>, Types)> {
        let mut found = Vec::new();
        pipeline::subqueries_in(on, &mut found);
        let mut plans = Plans::default();
        let mut seeded = Types::new();
        for e in found {
            let plan = Plan::of(
                asked,
                e,
                columns,
                made,
                casings,
                sided,
                Some("JOIN conditions"),
                Some(unseen),
            )?;
            if let (Asks::Scalar, Some(t)) = (&plan.asks, plan.first) {
                seeded.insert(e as *const Expr, t);
            }
            plans.plans.insert(e as *const Expr, plan);
        }
        Ok((plans, seeded))
    }

    /// Each subquery a VALUES list's values hold outside any other, read over no columns.
    pub(super) fn of_values(
        asked: Asked,
        rows: &[&'q Vec<Expr>],
        made: &Steps,
        casings: &Casings,
    ) -> Result<(Plans<'q>, Types)> {
        let mut found = Vec::new();
        for e in rows.iter().flat_map(|r| r.iter()) {
            pipeline::subqueries_in(e, &mut found);
        }
        let mut plans = Plans::default();
        let mut seeded = Types::new();
        for e in found {
            let plan = Plan::of(asked, e, &[], made, casings, false, None, None)?;
            if let (Asks::Scalar, Some(t)) = (&plan.asks, plan.first) {
                seeded.insert(e as *const Expr, t);
            }
            plans.plans.insert(e as *const Expr, plan);
        }
        Ok((plans, seeded))
    }

    /// The columns of the step its subqueries read on each group, where it groups.
    pub(super) fn around(&self) -> &[usize] {
        &self.around
    }

    /// The places of the columns a subquery cannot be read without, where it was read so.
    pub(super) fn read_by(&self, e: &Expr) -> &[usize] {
        self.plans
            .get(&(e as *const Expr))
            .map_or(&[], |p| p.reads_around.as_slice())
    }
}

impl<'q> Plan<'q> {
    /// `clause` the clause of the step it stands in, where an aggregate of the step's may not; where
    /// `on` holds the relations it cannot reference, it stands in a join's ON.
    #[allow(clippy::too_many_arguments)]
    fn of(
        asked: Asked,
        e: &'q Expr,
        columns: &[Col],
        made: &Steps,
        casings: &Casings,
        grouped: bool,
        clause: Option<&'static str>,
        on: Option<&[String]>,
    ) -> Result<Plan<'q>> {
        let arity = |x: &Expr| match x {
            Expr::Tuple(xs) => xs.len(),
            _ => 1,
        };
        let (query, asks, wants): (&Query, Asks, Option<usize>) = match e {
            Expr::Subquery(q) => (q, Asks::Scalar, None),
            Expr::Exists { subquery, negated } => {
                (subquery, Asks::Exists { negated: *negated }, None)
            }
            Expr::InSubquery {
                expr,
                subquery,
                negated,
            } => (
                subquery,
                Asks::Compared {
                    left: expr,
                    op: BinaryOperator::Eq,
                    any: true,
                    negated: *negated,
                },
                Some(arity(expr)),
            ),
            Expr::AnyOp {
                left,
                compare_op,
                right,
                ..
            }
            | Expr::AllOp {
                left,
                compare_op,
                right,
            } => {
                let Expr::Subquery(q) = right.as_ref() else {
                    unreachable!("a subquery")
                };
                (
                    q,
                    Asks::Compared {
                        left,
                        op: compare_op.clone(),
                        any: matches!(e, Expr::AnyOp { .. }),
                        negated: false,
                    },
                    Some(arity(left)),
                )
            }
            _ => unreachable!("a subquery"),
        };
        super::question::unlocked(query)?;
        // Read on each group, an aggregate it calls over the step's columns alone is the step's,
        // handed to it as a column of its own on each group.
        let own: Vec<(Option<&str>, &str)> = columns
            .iter()
            .filter(|c| pipeline::outer_level(c.qualifier.as_deref()) == 0)
            .map(|c| (c.qualifier.as_deref(), c.name.as_str()))
            .collect();
        let around =
            |q: &str, n: &str| !pipeline::naming(own.iter().copied(), Some(q), n).is_empty();
        let lifted_calls = match grouped {
            true => lifted(query, &around),
            false => Vec::new(),
        };
        let typed: Vec<super::typed::TypedColumn> = columns
            .iter()
            .map(|c| (c.qualifier.clone(), c.name.clone(), c.ty))
            .collect();
        let lifted_columns: Vec<Col> = lifted_calls
            .iter()
            .enumerate()
            .map(|(k, a)| {
                let ty = pipeline::typed_on(a, &typed, asked.release)
                    .ok()
                    .and_then(|t| t.get(&(*a as *const Expr)).copied());
                Col::handed(handed_name(k), ty)
            })
            .collect();
        let query: std::borrow::Cow<'q, Query> = match lifted_calls.is_empty() {
            true => std::borrow::Cow::Borrowed(query),
            false => {
                let mut q = query.clone();
                let mut handing = Handing {
                    lifted: &lifted_calls,
                    named: Vec::new(),
                };
                let _ = sqlparser::ast::VisitMut::visit(&mut q, &mut handing);
                std::borrow::Cow::Owned(q)
            }
        };
        let lifted: Vec<*const Expr> = lifted_calls.iter().map(|a| *a as *const Expr).collect();
        // Read for its columns alone: a sequence it draws from is drawn only as rows ask it.
        let _probing = super::sequences::probing();
        let empty = relations::Empty(asked.reads.catalog());
        let probe = Asked {
            reads: &empty,
            ..asked
        };
        // Read with the step's columns as the query around it, all of them, or all but one.
        let asked_around = |query_read: &Query, hidden: Option<usize>| {
            let kept = Kept::default();
            let mut columns: Vec<Col> = columns
                .iter()
                .chain(&lifted_columns)
                .map(relations::marked)
                .collect();
            if let Some(i) = hidden {
                columns[i].name.insert(0, '\u{3}');
            }
            let outer = Outer {
                values: vec![Value::Null; columns.len()],
                columns: columns.into(),
                kept: &kept,
                clause,
                unseen: on.map(<[String]>::to_vec).unwrap_or_default(),
            };
            relations::made_by(
                probe,
                query_read,
                made,
                casings,
                Some(&outer),
                &Accrued::default(),
            )
        };
        // An EXISTS reads whether its query makes a row, its select list never made where nothing
        // in the query counts its rows; read first as written, as PostgreSQL's parser reads it.
        let query = match (&asks, &query) {
            (Asks::Exists { .. }, std::borrow::Cow::Borrowed(q)) => {
                match exists_read(q, asked.release, casings) {
                    Some(read) => {
                        if relations::made_by(probe, q, made, casings, None, &Accrued::default())
                            .is_err()
                        {
                            asked_around(q, None)?;
                        }
                        std::borrow::Cow::Owned(read)
                    }
                    None => query,
                }
            }
            _ => query,
        };
        let query_read: &Query = &query;
        let none = relations::made_by(probe, query_read, made, casings, None, &Accrued::default());
        let (m, around, reads_around) = match none {
            Ok((m, _)) => (m, None, Vec::new()),
            Err(_) => {
                let (m, _) = asked_around(query_read, None)?;
                let ws = relations::words(&query_read.to_string());
                let named: Vec<usize> = (0..columns.len())
                    .filter(|&i| ws.contains(&columns[i].name))
                    .collect();
                // Where it is read on each group, or stands in a FULL JOIN's ON, the columns it
                // cannot be read without.
                let reads: Vec<usize> = match grouped {
                    true => named
                        .iter()
                        .copied()
                        .filter(|&i| asked_around(query_read, Some(i)).is_err())
                        .collect(),
                    false => Vec::new(),
                };
                (m, Some(named), reads)
            }
        };
        let width = m.rel.names.len();
        match (&asks, wants) {
            (Asks::Scalar, _) if width != 1 => {
                return Err(Error::new("subquery must return only one column"))
            }
            (_, Some(n)) if width > n => return Err(Error::new("subquery has too many columns")),
            (_, Some(n)) if width < n => return Err(Error::new("subquery has too few columns")),
            _ => {}
        }
        let first = m.types.first().copied().flatten();
        let in_order =
            query_read.with.is_none() && matches!(query_read.body.as_ref(), SetExpr::Select(_));
        let joinable = in_order
            && matches!(asks, Asks::Exists { .. })
            && around.is_some()
            && lifted.is_empty();
        drop(m);
        Ok(Plan {
            query,
            lifted,
            lifted_columns,
            asks,
            first,
            around,
            reads_around,
            once: RefCell::new(None),
            by_values: RefCell::new(HashMap::new()),
            written: RefCell::default(),
            refused: RefCell::default(),
            kept: Kept {
                in_order: std::cell::Cell::new(in_order),
                ..Kept::default()
            },
            read: OnceCell::new(),
            around_columns: OnceCell::new(),
            joinable,
        })
    }
}

/// What answers a step's subqueries, for each row it reads them on.
pub(super) struct Subs<'a, 'q> {
    plans: &'a Plans<'q>,
    asked: Asked<'a>,
    made: &'a Steps,
    casings: &'a Casings,
    /// The step's columns, those of the query around it among them.
    columns: &'a [Col],
    accrued: &'a Accrued,
    /// Each subquery's last step read off its text, once a round, by its plan.
    ready: RefCell<HashMap<*const Plan<'q>, Option<Rc<Readied<'a>>>>>,
}

/// Each set of values rows give, gathered while they fit the room.
struct Foreseen {
    seen: KeySet,
    sets: super::held::Columns,
    held: crate::room::Holding,
    over: bool,
}

impl Foreseen {
    fn new() -> Foreseen {
        Foreseen {
            seen: KeySet::default(),
            sets: Default::default(),
            held: crate::room::holding(0),
            over: false,
        }
    }

    /// The set of values a row more gives.
    fn see(&mut self, set: Vec<Value>) {
        if self.over {
            return;
        }
        if self.seen.ask(&keys_of(&set)).is_some() {
            return;
        }
        let w = self.sets.width_of(&set) + self.seen.width();
        self.seen.hold();
        match self.held.fits(w) {
            true => self.sets.push(set),
            false => self.over = true,
        }
    }

    /// The sets kept where they all fit.
    fn kept_in(self, kept: &Kept) {
        if !self.over {
            *kept.around.borrow_mut() = Some(self.sets);
            *kept.around_held.borrow_mut() = Some(self.held);
        }
    }
}

impl Kept {
    /// Each set of values `rows` give, `of` read on each, where they fit the room.
    pub(super) fn foresee(
        &self,
        rows: &Rows,
        of: &mut dyn FnMut(&[Value]) -> Vec<Value>,
    ) -> Result<()> {
        let mut foreseen = Foreseen::new();
        rows.each_chunk(&mut |c| {
            for r in &c {
                if foreseen.over {
                    break;
                }
                foreseen.see(of(r));
            }
            Ok(())
        })?;
        foreseen.kept_in(self);
        Ok(())
    }

    /// Lets go the sets of values foreseen, once its tables are read.
    fn let_go_around(&self) {
        *self.around.borrow_mut() = None;
        *self.around_held.borrow_mut() = None;
    }
}

impl<'a, 'q> Subs<'a, 'q> {
    pub(super) fn new(
        plans: &'a Plans<'q>,
        asked: Asked<'a>,
        made: &'a Steps,
        casings: &'a Casings,
        columns: &'a [Col],
        accrued: &'a Accrued,
    ) -> Subs<'a, 'q> {
        Subs {
            plans,
            asked,
            made,
            casings,
            columns,
            accrued,
            ready: RefCell::default(),
        }
    }

    /// Before any row asks: each set of values `rows` give what each subquery reading the query
    /// around it names, `around` the values of that query after each row's; then each EXISTS read
    /// as the join it is made, and where its rows pass the room, the truth of each of `rows` found.
    pub(super) fn foresee(&self, rows: &Rows, around: &[Value]) -> Result<()> {
        let plans: &'a Plans<'q> = self.plans;
        // One handed an aggregate of the step's reads values no row gives before it groups.
        let mut foreseen: Vec<(&Kept, &[usize], Foreseen)> = plans
            .plans
            .values()
            .filter(|p| p.lifted.is_empty())
            .filter_map(|p| Some((&p.kept, p.around.as_deref()?, Foreseen::new())))
            .collect();
        if !foreseen.is_empty() {
            rows.each_chunk(&mut |c| {
                for r in &c {
                    for (_, places, f) in foreseen.iter_mut().filter(|f| !f.2.over) {
                        f.see(places.iter().map(|&i| value_at(r, i, around)).collect());
                    }
                }
                Ok(())
            })?;
            for (kept, _, f) in foreseen {
                f.kept_in(kept);
            }
        }
        if rows.len() == 0 {
            return Ok(());
        }
        // Each EXISTS read as the join it is: its rows read; those past the room cut on their
        // keys' hash, and the step's rows no row held meets cut beside them, in one pass of all.
        let mut ahead = Vec::new();
        for plan in plans.plans.values() {
            let Some(readied) = self.joining(plan)? else {
                continue;
            };
            if readied.met.borrow().is_some() {
                continue;
            }
            match self.theirs(plan, &readied, true)? {
                (met, None) => *readied.met.borrow_mut() = Some(met),
                (met, Some(parts)) => ahead.push(Ahead {
                    places: plan.around.as_deref().expect("a subquery reading around it"),
                    asking: parts.like(),
                    theirs: parts,
                    bits: vec![0u64; rows.len().div_ceil(64)],
                    bits_held: crate::room::holding(rows.len().div_ceil(64) as u64 * 8),
                    readied,
                    met,
                }),
            }
        }
        if ahead.is_empty() {
            return Ok(());
        }
        let mut seq = 0usize;
        rows.each_chunk(&mut |c| {
            for r in c {
                for a in ahead.iter_mut() {
                    self.asks(a, seq, &r, around)?;
                }
                seq += 1;
            }
            Ok(())
        })?;
        // Every row held let go before any part is met.
        for a in ahead.iter_mut() {
            a.met.keys = KeySet::default();
            a.met.rows = KeyRows::default();
            a.met.pairs = Columns::default();
            a.met.held = None;
        }
        for mut a in ahead {
            let theirs = a.theirs.into_parts(u64::MAX)?;
            let ours = a.asking.into_parts(u64::MAX)?;
            for ((lo, hi, t), (_, _, o)) in theirs.into_iter().zip(ours) {
                self.meet_part(&a.readied, t, o, (lo, hi), 0, &mut a.bits)?;
            }
            a.met.ahead = Some((a.bits, a.bits_held));
            *a.readied.met.borrow_mut() = Some(a.met);
        }
        Ok(())
    }

    /// A row of the step, its `seq`th, asking an EXISTS whose rows passed the room: true where a
    /// row it holds meets it; else written by its key's hash beside its place, to meet the rows of
    /// the part of that hash.
    fn asks(&self, a: &mut Ahead<'a>, seq: usize, r: &[Value], around: &[Value]) -> Result<()> {
        let joined = a.readied.joined.as_ref().expect("read as a join");
        let ready = &a.readied.ready;
        let values: Vec<Value> = a.places.iter().map(|&i| value_at(r, i, around)).collect();
        let Some(tie) = ready.joined_values(joined, self.asked, &values)? else {
            return Ok(());
        };
        let key = keys_of(&tie);
        let found = a.met.meets(&key, joined.by_pairs(), &mut |h| {
            ready.pair_holds(joined, self.asked, h, &values)
        })?;
        if found {
            a.bits[seq / 64] |= 1 << (seq % 64);
            return Ok(());
        }
        let mut out = vec![Value::Int(seq as i64)];
        out.extend(tie);
        if joined.by_pairs() {
            out.extend(values);
        }
        a.asking.push(pipeline::parted::key_hash(&key, 0), out)
    }

    /// While the step reads the chunk of `rows` from its `at`th row: the truth found ahead of each,
    /// by its keys, for each EXISTS read as the join it is whose rows passed the room.
    pub(super) fn read_ahead(
        &self,
        rows: &[Vec<Value>],
        at: usize,
        around: &[Value],
    ) -> Result<()> {
        for plan in self.plans.plans.values() {
            let found = self.ready.borrow().get(&(plan as *const Plan)).cloned();
            let Some(Some(readied)) = found else {
                continue;
            };
            let Some(joined) = &readied.joined else {
                continue;
            };
            let mut met = readied.met.borrow_mut();
            let Some(m) = met.as_mut() else {
                continue;
            };
            let Some((bits, _)) = &m.ahead else {
                continue;
            };
            let places = plan.around.as_ref().expect("a subquery reading around it");
            let mut reading = HashMap::with_capacity(rows.len());
            for (j, r) in rows.iter().enumerate() {
                let values: Vec<Value> = places.iter().map(|&i| value_at(r, i, around)).collect();
                if let Some(tie) = readied.ready.joined_values(joined, self.asked, &values)? {
                    let seq = at + j;
                    let truth = bits.get(seq / 64).is_some_and(|w| w >> (seq % 64) & 1 == 1);
                    reading.insert(reading_key(&keys_of(&tie), &values, joined.by_pairs()), truth);
                }
            }
            m.reading = reading;
        }
        Ok(())
    }

    /// What a subquery returns for a row: once, where it reads nothing of the query around it; else
    /// for the values the row gives what it names, and the group the aggregates of the step's it is
    /// handed, the query around it read from those alone.
    fn returned(
        &self,
        plan: &'a Plan<'q>,
        row: &[Value],
        computed: &Computed,
    ) -> Result<Rc<Returned>> {
        let asked = Asked {
            answer_kept: true,
            ..self.asked
        };
        let Some(places) = &plan.around else {
            if let Some(r) = plan.once.borrow().as_ref() {
                return Ok(r.clone());
            }
            // Its one-time conditions first, as its plan starts: where they shut out every row, it
            // is read over tables holding none.
            let text = plan.query.to_string();
            let shut = super::folded::gated(
                self.asked.reads.catalog(),
                &text,
                Some(self.asked.reads),
            )? && !crate::pgcatalog::names_catalog(&text);
            let empty = relations::Empty(self.asked.reads.catalog());
            let asked = match shut {
                true => Asked {
                    reads: &empty,
                    ..asked
                },
                false => asked,
            };
            let (m, _) = relations::made_by(
                asked,
                &plan.query,
                self.made,
                self.casings,
                None,
                self.accrued,
            )?;
            let r = Returned::of(&plan.asks, m)?;
            *plan.once.borrow_mut() = Some(r.clone());
            return Ok(r);
        };
        let mut values: Vec<Value> = places.iter().map(|&i| row[i].clone()).collect();
        for &a in &plan.lifted {
            values.push(computed.of(a).ok_or_else(|| {
                Error::new("an aggregate of the query around a subquery is not read here")
            })?);
        }
        let key = values_key(&values);
        if let Some(r) = self.answered(plan, &key)? {
            return Ok(r);
        }
        let r = self.answer_for(plan, values)?;
        if plan.kept.answers_now.replace(false) {
            // Its table lies past the room: every answer is made now, in the order of its rows.
            let sets = plan.kept.around.borrow().clone().unwrap_or_default();
            for set in sets.rows() {
                let k = values_key(&set);
                match self.answer_for(plan, set) {
                    Ok(r) => self.keep_answer(plan, k, r, true)?,
                    Err(e) => {
                        plan.refused.borrow_mut().insert(k, e.to_string());
                    }
                }
            }
            return self
                .answered(plan, &key)?
                .ok_or_else(|| Error::new("a subquery's answer was not foreseen"));
        }
        self.keep_answer(plan, key, r.clone(), false)?;
        Ok(r)
    }

    /// The answer kept for a set of values, read back where it was written.
    fn answered(&self, plan: &Plan, key: &str) -> Result<Option<Rc<Returned>>> {
        if let Some(r) = plan.by_values.borrow().get(key) {
            return Ok(Some(r.clone()));
        }
        if let Some(e) = plan.refused.borrow().get(key) {
            return Err(Error::new(e.clone()));
        }
        let written = plan.written.borrow();
        let (Some(run), Some(&(at, n))) = (&written.run, written.at.get(key)) else {
            return Ok(None);
        };
        Ok(Some(Returned::of_rows(run.read_at(at, n)?)))
    }

    /// The subquery made for a set of values: its last step run for them, where its statement is
    /// read once a round; else its statement made for them.
    fn answer_for(&self, plan: &'a Plan<'q>, values: Vec<Value>) -> Result<Rc<Returned>> {
        let outer = Outer {
            columns: std::borrow::Cow::Borrowed(self.around_columns(plan)),
            values,
            kept: &plan.kept,
            clause: None,
            unseen: Vec::new(),
        };
        // A FROM read once a round is read from the file; one made for each set of values from what
        // its first making read.
        let cached = Cached {
            reads: self.asked.reads,
            kept: &plan.kept,
        };
        if let Some(readied) = self.ready(plan)? {
            let reads: &dyn Reads = match readied.ready.reads_once() {
                true => self.asked.reads,
                false => &cached,
            };
            let asked = Asked {
                reads,
                answer_kept: true,
                ..self.asked
            };
            let m = readied.ready.run(asked, Some(&outer), self.accrued, None)?;
            return Returned::of(&plan.asks, m);
        }
        let asked = Asked {
            reads: &cached,
            answer_kept: true,
            ..self.asked
        };
        let (m, _) = relations::made_by(
            asked,
            &plan.query,
            self.made,
            self.casings,
            Some(&outer),
            self.accrued,
        )?;
        Returned::of(&plan.asks, m)
    }

    /// The columns of the query around a subquery reading it, as the subquery reads them.
    fn around_columns(&self, plan: &'a Plan<'q>) -> &'a [Col] {
        plan.around_columns.get_or_init(|| {
            let places = plan.around.as_ref().expect("a subquery reading around it");
            places
                .iter()
                .map(|&i| &self.columns[i])
                .chain(&plan.lifted_columns)
                .map(relations::marked)
                .collect()
        })
    }

    /// The query around a subquery as it reads it, before any row gives it values.
    fn outer_unvalued(&self, plan: &'a Plan<'q>) -> Outer<'a> {
        let columns = self.around_columns(plan);
        Outer {
            columns: std::borrow::Cow::Borrowed(columns),
            values: vec![Value::Null; columns.len()],
            kept: &plan.kept,
            clause: None,
            unseen: Vec::new(),
        }
    }

    /// A subquery's last step read off its text, once a round; none where its statement is not
    /// read so.
    fn ready(&self, plan: &'a Plan<'q>) -> Result<Option<Rc<Readied<'a>>>> {
        let at = plan as *const Plan<'q>;
        if let Some(r) = self.ready.borrow().get(&at) {
            return Ok(r.clone());
        }
        let outer = self.outer_unvalued(plan);
        let asked = Asked {
            answer_kept: true,
            ..self.asked
        };
        if plan.read.get().is_none() {
            let once = relations::Once::of(
                asked,
                &plan.query,
                self.made,
                self.casings,
                &outer,
                self.accrued,
            )?;
            let _ = plan.read.set(once);
        }
        let readied = match plan.read.get() {
            Some(Some(once)) => {
                let ready = once.ready(asked, &outer, self.accrued)?;
                let joined = match plan.joinable {
                    true => ready.joined(self.asked.release),
                    false => None,
                };
                Some(Rc::new(Readied {
                    ready,
                    joined,
                    met: RefCell::new(None),
                }))
            }
            _ => None,
        };
        self.ready.borrow_mut().insert(at, readied.clone());
        Ok(readied)
    }

    /// An EXISTS's subquery read off its text, where it is read as the join it is.
    fn joining(&self, plan: &'a Plan<'q>) -> Result<Option<Rc<Readied<'a>>>> {
        if !plan.joinable {
            return Ok(None);
        }
        Ok(self.ready(plan)?.filter(|r| r.joined.is_some()))
    }

    /// Where an EXISTS is read as the join it is: whether a row of its subquery meets the row
    /// asking; none where it is not read so, or where its rows passed the room and this row's truth
    /// is not held.
    fn exists(&self, plan: &'a Plan<'q>, row: &[Value]) -> Result<Option<bool>> {
        let Some(readied) = self.joining(plan)? else {
            return Ok(None);
        };
        let joined = readied.joined.as_ref().expect("read as a join");
        let places = plan.around.as_ref().expect("a subquery reading around it");
        let values: Vec<Value> = places.iter().map(|&i| row[i].clone()).collect();
        let Some(tie) = readied.ready.joined_values(joined, self.asked, &values)? else {
            return Ok(Some(false));
        };
        let key = keys_of(&tie);
        if readied.met.borrow().is_none() {
            let (met, _) = self.theirs(plan, &readied, false)?;
            *readied.met.borrow_mut() = Some(met);
        }
        let met = readied.met.borrow();
        let met = met.as_ref().expect("met");
        if met.ahead.is_some() {
            let k = reading_key(&key, &values, joined.by_pairs());
            return Ok(met.reading.get(&k).copied());
        }
        if met.past {
            return Ok(None);
        }
        let ready = &readied.ready;
        met.meets(&key, joined.by_pairs(), &mut |r| {
            ready.pair_holds(joined, self.asked, r, &values)
        })
        .map(Some)
    }

    /// An EXISTS's subquery's rows, read as the join it is: held where the room holds them. Where
    /// they pass it and the step's rows are read ahead (`ahead`), the rest cut on their keys' hash,
    /// each written once, to be read back once with the part of the step's rows of those hashes.
    fn theirs(
        &self,
        plan: &'a Plan<'q>,
        readied: &Readied<'a>,
        ahead: bool,
    ) -> Result<(Met, Option<super::spill::Parts>)> {
        let joined = readied.joined.as_ref().expect("read as a join");
        let ready = &readied.ready;
        let by_pairs = joined.by_pairs();
        let outer = self.outer_unvalued(plan);
        let asked = Asked {
            answer_kept: true,
            ..self.asked
        };
        let mut met = Met::default();
        let mut held = crate::room::holding(0);
        let mut parts: Option<super::spill::Parts> = None;
        let (mut n, mut bytes, mut held_bytes) = (0u64, 0u64, 0u64);
        let most = std::cell::Cell::new(None);
        ready.joined_rows(joined, asked, &outer, self.accrued, &most, &mut |tie, row| {
            let w = super::spill::row_width(&tie) + super::spill::row_width(&row);
            n += 1;
            bytes += w;
            let key = keys_of(&tie);
            if parts.is_none() {
                if met.past {
                    return Ok(());
                }
                let h = met.width(&key, &row, by_pairs);
                if held.fits(h) {
                    held_bytes += h;
                    met.hold(key, row, by_pairs);
                    return Ok(());
                }
                met.past = true;
                if !ahead {
                    return Ok(());
                }
                // The rows to come cut into parts each of which fits half the room the rows held
                // leave once let go, as a join's batches are.
                let room = crate::room::current();
                let free = room.as_ref().map_or(u64::MAX, |r| r.free_bytes()) + held_bytes;
                let most_parts = room
                    .as_ref()
                    .map_or(1, |r| r.sizes().bytes / (2 * crate::cut::PAGE as u64))
                    .max(8);
                let count = match most.get() {
                    Some(m) => {
                        let to_come = m.saturating_sub(n - 1).saturating_mul(bytes / n);
                        to_come / free.max(super::spill::CHUNK).div_ceil(2) + 1
                    }
                    None => PARTS_UNKNOWN,
                };
                parts = Some(super::spill::Parts::of_count(
                    0,
                    u64::MAX,
                    count.min(most_parts),
                ));
            }
            let mut out = tie;
            out.extend(row);
            parts
                .as_ref()
                .expect("parts")
                .push(pipeline::parted::key_hash(&key, 0), out)
        })?;
        plan.kept.let_go_around();
        met.held = Some(held);
        Ok((met, parts))
    }

    /// One part of an EXISTS's rows met with the part of the step's rows that asks of the same
    /// hashes: its rows held and the step's read back, each one's truth found; where its rows pass
    /// the room, both parts cut again on their hashes.
    fn meet_part(
        &self,
        readied: &Readied<'a>,
        theirs: super::spill::Run,
        ours: super::spill::Run,
        (lo, hi): (u64, u64),
        cut: u32,
        bits: &mut [u64],
    ) -> Result<()> {
        if ours.len() == 0 || theirs.len() == 0 {
            return Ok(());
        }
        let joined = readied.joined.as_ref().expect("read as a join");
        let ready = &readied.ready;
        let (k, by_pairs) = (joined.ties(), joined.by_pairs());
        let free = crate::room::current()
            .map_or(u64::MAX, |r| r.free_bytes())
            .max(super::spill::CHUNK);
        if theirs.total() > free && cut < CUTS && hi > lo {
            let n = theirs.total() / free.div_ceil(2) + 1;
            let again = super::spill::Parts::of_count(lo, hi, n);
            let asking = again.like();
            theirs.each_chunk(&mut |c| {
                for r in c {
                    again.push(pipeline::parted::key_hash(&keys_of(&r[..k]), 0), r)?;
                }
                Ok(())
            })?;
            ours.each_chunk(&mut |c| {
                for r in c {
                    asking.push(pipeline::parted::key_hash(&keys_of(&r[1..1 + k]), 0), r)?;
                }
                Ok(())
            })?;
            drop((theirs, ours));
            let theirs = again.into_parts(hi)?;
            let ours = asking.into_parts(hi)?;
            for ((lo, hi, t), (_, _, o)) in theirs.into_iter().zip(ours) {
                self.meet_part(readied, t, o, (lo, hi), cut + 1, bits)?;
            }
            return Ok(());
        }
        let mut met = Met {
            held: Some(crate::room::holding(theirs.total())),
            ..Met::default()
        };
        theirs.each_chunk(&mut |c| {
            for mut r in c {
                let row = r.split_off(k);
                met.hold(keys_of(&r), row, by_pairs);
            }
            Ok(())
        })?;
        drop(theirs);
        ours.each_chunk(&mut |c| {
            for r in c {
                let Value::Int(seq) = r[0] else {
                    continue;
                };
                let seq = seq as usize;
                let values = &r[1 + k..];
                let found = met.meets(&keys_of(&r[1..1 + k]), by_pairs, &mut |h| {
                    ready.pair_holds(joined, self.asked, h, values)
                })?;
                if found {
                    bits[seq / 64] |= 1 << (seq % 64);
                }
            }
            Ok(())
        })
    }

    /// An answer kept for the rows that ask for its values again, where the room has room for it;
    /// past it, written where nothing could make it again, else made again for each that asks.
    fn keep_answer(&self, plan: &Plan, key: String, r: Rc<Returned>, must: bool) -> Result<()> {
        if crate::room::current().is_none_or(|room| !room.over()) {
            plan.by_values.borrow_mut().insert(key, r);
            return Ok(());
        }
        if !must {
            return Ok(());
        }
        let mut written = plan.written.borrow_mut();
        let w = &mut *written;
        let run = w.run.get_or_insert_with(Run::past);
        let rows = match &r.past {
            Some(p) => p.all()?,
            None => r.rows.clone(),
        };
        let at = run.write_at(&rows)?;
        w._held
            .get_or_insert_with(|| crate::room::holding(0))
            .more(key.len() as u64 + 12);
        w.at.insert(key, (at, rows.len() as u32));
        Ok(())
    }
}

/// An EXISTS's query as PostgreSQL's planner reads it before it plans it (simplify_EXISTS_query):
/// one SELECT that folds nothing, makes no set, has no HAVING, OFFSET or locking clause, writes
/// nothing, and whose LIMIT cuts no row, read without its select list, its GROUP BY, WINDOW,
/// DISTINCT, ORDER BY and LIMIT; none where it is any other.
fn exists_read(q: &Query, release: u8, casings: &Casings) -> Option<Query> {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    if s.having.is_some() || !q.locks.is_empty() || q.for_clause.is_some() {
        return None;
    }
    if q.with
        .as_ref()
        .is_some_and(|w| w.cte_tables.iter().any(|c| relations::writes(&c.query)))
    {
        return None;
    }
    let GroupByExpr::Expressions(grouped, modifiers) = &s.group_by else {
        return None;
    };
    if !modifiers.is_empty()
        || grouped.iter().any(|g| {
            matches!(g, Expr::Rollup(_) | Expr::Cube(_) | Expr::GroupingSets(_))
        })
    {
        return None;
    }
    let items = s.projection.iter().filter_map(|i| match i {
        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => Some(e),
        _ => None,
    });
    let sorted = match q.order_by.as_ref().map(|o| &o.kind) {
        Some(OrderByKind::Expressions(os)) => os.iter().map(|o| &o.expr).collect(),
        _ => Vec::new(),
    };
    if items.chain(sorted).any(pipeline::folds_or_makes_sets)
        || !pipeline::cuts_no_row(q, release, casings)
    {
        return None;
    }
    let mut read = q.clone();
    read.order_by = None;
    read.limit_clause = None;
    read.fetch = None;
    if let SetExpr::Select(s) = read.body.as_mut() {
        s.projection = vec![SelectItem::UnnamedExpr(Expr::Value(
            sqlparser::ast::Value::Boolean(true).into(),
        ))];
        s.group_by = GroupByExpr::Expressions(Vec::new(), Vec::new());
        s.named_window = Vec::new();
        s.distinct = None;
    }
    Some(read)
}

/// A set of values, as answers are kept by.
fn values_key(values: &[Value]) -> String {
    values
        .iter()
        .map(|v| format!("{v:?}"))
        .collect::<Vec<_>>()
        .join("\0")
}

/// A truth's negation, NULL staying NULL.
fn not(v: Value) -> Value {
    match v {
        Value::Bool(b) => Value::Bool(!b),
        other => other,
    }
}

/// Whether an = between values of two types holds exactly where their keys are one.
fn keyed_alike(a: Option<ColumnType>, b: Option<ColumnType>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => {
            (a.is_integer() && b.is_integer())
                || matches!((a, b), (ColumnType::Text(_), ColumnType::Text(_)))
                || (a == b && a == ColumnType::Uuid)
        }
        _ => false,
    }
}

/// A row compared with each row a subquery returned, by = or <>, field by field: true where one (or
/// each) holds, NULL where none (or not every one) holds and one compared NULL.
fn rows_compared(
    op: &BinaryOperator,
    xs: &[Value],
    lefts: &[Expr],
    rows: &[Vec<Value>],
    any: bool,
) -> Result<Value> {
    if !matches!(op, BinaryOperator::Eq | BinaryOperator::NotEq) {
        return Err(Error::new(format!(
            "a row compared by {op} with a subquery is not read"
        )));
    }
    let mut unknown = false;
    for r in rows {
        let mut row = Some(true);
        for ((x, left), v) in xs.iter().zip(lefts).zip(r) {
            match pipeline::compared_each(
                &BinaryOperator::Eq,
                x.clone(),
                std::iter::once(v.clone()),
                left,
                true,
            )? {
                Value::Bool(false) => {
                    row = Some(false);
                    break;
                }
                Value::Bool(true) => {}
                _ => row = None,
            }
        }
        if *op == BinaryOperator::NotEq {
            row = row.map(|b| !b);
        }
        match row {
            Some(true) if any => return Ok(Value::Bool(true)),
            Some(false) if !any => return Ok(Value::Bool(false)),
            None => unknown = true,
            _ => {}
        }
    }
    Ok(match unknown {
        true => Value::Null,
        false => Value::Bool(!any),
    })
}

impl pipeline::Subqueries for Subs<'_, '_> {
    fn value(&self, e: &Expr, row: &[Value], frame: &Frame, computed: &Computed) -> Result<Value> {
        let plan = self
            .plans
            .plans
            .get(&(e as *const Expr))
            .ok_or_else(|| Error::new(format!("{e} is not read here")))?;
        if let Asks::Exists { negated } = &plan.asks {
            if let Some(found) = self.exists(plan, row)? {
                return Ok(Value::Bool(found != *negated));
            }
        }
        let returned = self.returned(plan, row, computed)?;
        let rows = &returned.rows;
        match &plan.asks {
            Asks::Scalar => match rows.as_slice() {
                [] => Ok(Value::Null),
                [r] => Ok(r[0].clone()),
                _ => Err(Error::new(
                    "more than one row returned by a subquery used as an expression",
                )),
            },
            Asks::Exists { negated } => Ok(Value::Bool(returned.is_empty() == *negated)),
            Asks::Compared {
                left,
                op,
                any,
                negated,
            } => {
                let v = match left {
                    Expr::Tuple(lefts) => {
                        let xs = lefts
                            .iter()
                            .map(|x| pipeline::eval(x, row, frame, computed))
                            .collect::<Result<Vec<_>>>()?;
                        returned.compared(*any, &mut |c| rows_compared(op, &xs, lefts, c, *any))?
                    }
                    _ => {
                        let x = pipeline::eval(left, row, frame, computed)?;
                        let left_type = frame.types.get(&(*left as *const Expr)).copied();
                        let keyed = (*op == BinaryOperator::Eq
                            && *any
                            && keyed_alike(left_type, plan.first))
                        .then(|| returned.keys())
                        .flatten();
                        if let Some((keys, nulls)) = keyed {
                            match () {
                                _ if returned.is_empty() => Value::Bool(false),
                                _ if x.is_null() => Value::Null,
                                _ if keys.contains(&[pipeline::key(&x)]) => Value::Bool(true),
                                _ if nulls => Value::Null,
                                _ => Value::Bool(false),
                            }
                        } else {
                            returned.compared(*any, &mut |c| {
                                pipeline::compared_each(
                                    op,
                                    x.clone(),
                                    c.iter().map(|r| r[0].clone()),
                                    left,
                                    *any,
                                )
                            })?
                        }
                    }
                };
                Ok(match negated {
                    true => not(v),
                    false => v,
                })
            }
        }
    }
}

/// Whether a question holds a subquery: in an expression of any of its steps, or as a LATERAL
/// subquery in a FROM.
pub(super) fn held(q: &Query) -> bool {
    fn expr(e: &Expr) -> bool {
        let mut found = Vec::new();
        pipeline::subqueries_in(e, &mut found);
        !found.is_empty()
    }
    fn factor(t: &TableFactor) -> bool {
        match t {
            TableFactor::Derived { lateral: true, .. } => true,
            TableFactor::Derived { subquery, .. } => held(subquery),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                factor(&table_with_joins.relation)
                    || table_with_joins.joins.iter().any(|j| factor(&j.relation))
            }
            _ => false,
        }
    }
    fn body(b: &SetExpr) -> bool {
        match b {
            SetExpr::Select(s) => {
                s.projection.iter().any(|i| match i {
                    SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                        expr(e)
                    }
                    _ => false,
                }) || s.selection.as_ref().is_some_and(expr)
                    || s.having.as_ref().is_some_and(expr)
                    || matches!(&s.group_by, GroupByExpr::Expressions(g, _) if g.iter().any(expr))
                    || s.from.iter().any(|t| {
                        factor(&t.relation)
                            || t.joins.iter().any(|j| {
                                factor(&j.relation)
                                    || relations::join_on(&j.join_operator).is_some_and(expr)
                            })
                    })
            }
            SetExpr::Query(q) => held(q),
            SetExpr::SetOperation { left, right, .. } => body(left) || body(right),
            _ => false,
        }
    }
    q.with
        .as_ref()
        .is_some_and(|w| w.cte_tables.iter().any(|c| held(&c.query)))
        || body(&q.body)
        || matches!(
            q.order_by.as_ref().map(|o| &o.kind),
            Some(OrderByKind::Expressions(os)) if os.iter().any(|o| expr(&o.expr))
        )
}

/// The subqueries an expression read after its step groups holds, those inside an aggregate's
/// arguments read on each row before it.
fn on_groups<'q>(e: &'q Expr, after: &mut Vec<&'q Expr>, before: &mut Vec<&'q Expr>) {
    match e {
        Expr::Function(f)
            if f.over.is_none()
                && (pipeline::is_aggregate(&pipeline::function_name(f))
                    || !f.within_group.is_empty()) =>
        {
            pipeline::subqueries_in(e, before)
        }
        e if pipeline::is_subquery(e) => {
            if let Expr::InSubquery { expr, .. }
            | Expr::AnyOp { left: expr, .. }
            | Expr::AllOp { left: expr, .. } = e
            {
                on_groups(expr, after, before);
            }
            after.push(e);
        }
        e => {
            let mut parts = pipeline::parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            for p in parts {
                on_groups(p, after, before);
            }
        }
    }
}

/// The name of the column the `k`th aggregate of the query around a subquery is handed to it as,
/// one no query can write.
fn handed_name(k: usize) -> String {
    format!("\u{e}around{k}")
}

/// The subqueries an expression holds that its step reads on each group.
pub(super) fn read_on_groups(e: &Expr) -> Vec<&Expr> {
    let (mut after, mut before) = (Vec::new(), Vec::new());
    on_groups(e, &mut after, &mut before);
    after
}

/// The query a subquery expression asks.
pub(super) fn asked_query(e: &Expr) -> Option<&Query> {
    match e {
        Expr::Subquery(q)
        | Expr::Exists { subquery: q, .. }
        | Expr::InSubquery { subquery: q, .. } => Some(q),
        Expr::AnyOp { right, .. } | Expr::AllOp { right, .. } => match right.as_ref() {
            Expr::Subquery(q) => Some(q),
            _ => None,
        },
        _ => None,
    }
}

/// The aggregates a subquery's own clauses call over the query around it alone, which that query
/// computes on each of its groups and hands the subquery, as PostgreSQL's parser gives an aggregate
/// to the query its columns are of: each column it reads qualified by a name `around` gives a
/// relation of that query, and by none of the subquery's FROM. In the order its words hold them.
pub(super) fn lifted<'q>(q: &'q Query, around: &dyn Fn(&str, &str) -> bool) -> Vec<&'q Expr> {
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Vec::new();
    };
    let mut own: Vec<String> = Vec::new();
    from_names(&s.from, &mut own);
    let mut ons: Vec<&Expr> = Vec::new();
    for t in &s.from {
        for j in &t.joins {
            if let Some(sqlparser::ast::JoinConstraint::On(e)) = join_on(&j.join_operator) {
                ons.push(e);
            }
        }
    }
    let of_around = |e: &Expr| {
        let mut read = Vec::new();
        pipeline::columns_read(e, &mut read);
        !read.is_empty()
            && read.iter().all(|(q, n)| {
                q.as_deref().is_some_and(|q| {
                    let last = q.rsplit('.').next().unwrap_or(q);
                    !own.iter().any(|o| o == last) && around(q, n)
                })
            })
    };
    let mut roots: Vec<&Expr> = Vec::new();
    for item in &s.projection {
        if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = item {
            roots.push(e);
        }
    }
    roots.extend(ons);
    roots.extend(s.selection.as_ref());
    if let GroupByExpr::Expressions(g, _) = &s.group_by {
        roots.extend(g);
    }
    roots.extend(s.having.as_ref());
    if let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) {
        roots.extend(os.iter().map(|o| &o.expr));
    }
    let mut out = Vec::new();
    for r in roots {
        aggregates_of(r, &of_around, &mut out);
    }
    out
}

/// The aggregates an expression calls, a subquery's apart, that `keep` keeps.
fn aggregates_of<'q>(e: &'q Expr, keep: &dyn Fn(&Expr) -> bool, out: &mut Vec<&'q Expr>) {
    match e {
        Expr::Function(f)
            if f.over.is_none()
                && (pipeline::is_aggregate(&pipeline::function_name(f))
                    || !f.within_group.is_empty()) =>
        {
            if keep(e) {
                out.push(e);
            }
        }
        e => {
            let mut parts = pipeline::parts_of(e);
            if parts.is_empty() {
                parts = super::collate::children(e);
            }
            for p in parts {
                aggregates_of(p, keep, out);
            }
        }
    }
}

/// The names a FROM's relations go by, those of a parenthesized join with no alias among them.
fn from_names(from: &[sqlparser::ast::TableWithJoins], out: &mut Vec<String>) {
    for t in from {
        for f in std::iter::once(&t.relation).chain(t.joins.iter().map(|j| &j.relation)) {
            match f {
                TableFactor::NestedJoin {
                    table_with_joins,
                    alias: None,
                } => from_names(std::slice::from_ref(table_with_joins.as_ref()), out),
                f => out.extend(relation_name(f)),
            }
        }
    }
}

/// The names the relations of a query's own FROM go by, each arm's of a set operation.
fn query_names(q: &Query) -> Vec<String> {
    fn of_body(b: &SetExpr, out: &mut Vec<String>) {
        match b {
            SetExpr::Select(s) => from_names(&s.from, out),
            SetExpr::SetOperation { left, right, .. } => {
                of_body(left, out);
                of_body(right, out);
            }
            _ => {}
        }
    }
    let mut out = Vec::new();
    of_body(&q.body, &mut out);
    out
}

/// A subquery's query with each aggregate of the query around it written as the column it is
/// handed as: in its own clauses, and in a query inside it whose relations hide none of the
/// aggregate's.
struct Handing<'a, 'q> {
    lifted: &'a [&'q Expr],
    /// The names of each query's relations the walk is inside, the subquery's first.
    named: Vec<Vec<String>>,
}

impl sqlparser::ast::VisitorMut for Handing<'_, '_> {
    type Break = ();

    fn pre_visit_query(&mut self, q: &mut Query) -> std::ops::ControlFlow<()> {
        self.named.push(query_names(q));
        std::ops::ControlFlow::Continue(())
    }

    fn post_visit_query(&mut self, _: &mut Query) -> std::ops::ControlFlow<()> {
        self.named.pop();
        std::ops::ControlFlow::Continue(())
    }

    fn post_visit_expr(&mut self, e: &mut Expr) -> std::ops::ControlFlow<()> {
        let Some(k) = self.lifted.iter().position(|a| **a == *e) else {
            return std::ops::ControlFlow::Continue(());
        };
        let mut read = Vec::new();
        pipeline::columns_read(e, &mut read);
        let hidden = self.named.iter().skip(1).flatten().any(|n| {
            read.iter().any(|(q, _)| {
                q.as_deref()
                    .is_some_and(|q| q.rsplit('.').next().unwrap_or(q) == n)
            })
        });
        if !hidden {
            *e = Expr::Identifier(sqlparser::ast::Ident::with_quote('"', handed_name(k)));
        }
        std::ops::ControlFlow::Continue(())
    }
}

/// The name a FROM item goes by: its alias, or a table's own name.
fn relation_name(f: &TableFactor) -> Option<String> {
    match f {
        TableFactor::Table { alias: Some(a), .. }
        | TableFactor::Derived { alias: Some(a), .. }
        | TableFactor::Function { alias: Some(a), .. }
        | TableFactor::TableFunction { alias: Some(a), .. }
        | TableFactor::UNNEST { alias: Some(a), .. }
        | TableFactor::NestedJoin { alias: Some(a), .. } => Some(crate::catalog::ident(&a.name)),
        TableFactor::Table { name, .. } => {
            let n = crate::catalog::object_name(name);
            Some(n.rsplit('.').next().unwrap_or(&n).to_string())
        }
        _ => None,
    }
}

/// A join's constraint, where its kind has one.
fn join_on(j: &sqlparser::ast::JoinOperator) -> Option<&sqlparser::ast::JoinConstraint> {
    use sqlparser::ast::JoinOperator as J;
    match j {
        J::Join(c)
        | J::Inner(c)
        | J::Left(c)
        | J::LeftOuter(c)
        | J::Right(c)
        | J::RightOuter(c)
        | J::FullOuter(c)
        | J::CrossJoin(c) => Some(c),
        _ => None,
    }
}

/// Calls `f` on each subquery an expression holds outside any other.
pub(super) fn each_query_mut(e: &mut Expr, f: &mut dyn FnMut(&mut Query)) {
    use sqlparser::ast::{FunctionArg, FunctionArgExpr, FunctionArguments};
    match e {
        Expr::Subquery(q) | Expr::Exists { subquery: q, .. } => f(q),
        Expr::InSubquery { expr, subquery, .. } => {
            each_query_mut(expr, f);
            f(subquery);
        }
        Expr::BinaryOp { left, right, .. }
        | Expr::AnyOp { left, right, .. }
        | Expr::AllOp { left, right, .. }
        | Expr::IsDistinctFrom(left, right)
        | Expr::IsNotDistinctFrom(left, right)
        | Expr::Like {
            expr: left,
            pattern: right,
            ..
        }
        | Expr::ILike {
            expr: left,
            pattern: right,
            ..
        } => {
            each_query_mut(left, f);
            each_query_mut(right, f);
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Nested(expr)
        | Expr::Cast { expr, .. }
        | Expr::Collate { expr, .. }
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::IsTrue(expr)
        | Expr::IsNotTrue(expr)
        | Expr::IsFalse(expr)
        | Expr::IsNotFalse(expr)
        | Expr::IsUnknown(expr)
        | Expr::IsNotUnknown(expr) => each_query_mut(expr, f),
        Expr::InList { expr, list, .. } => {
            each_query_mut(expr, f);
            list.iter_mut().for_each(|x| each_query_mut(x, f));
        }
        Expr::Between {
            expr, low, high, ..
        } => {
            each_query_mut(expr, f);
            each_query_mut(low, f);
            each_query_mut(high, f);
        }
        Expr::Case {
            operand,
            conditions,
            else_result,
            ..
        } => {
            if let Some(o) = operand {
                each_query_mut(o, f);
            }
            for c in conditions {
                each_query_mut(&mut c.condition, f);
                each_query_mut(&mut c.result, f);
            }
            if let Some(x) = else_result {
                each_query_mut(x, f);
            }
        }
        Expr::Function(fun) => {
            if let FunctionArguments::List(l) = &mut fun.args {
                for a in &mut l.args {
                    if let FunctionArg::Unnamed(FunctionArgExpr::Expr(x))
                    | FunctionArg::Named {
                        arg: FunctionArgExpr::Expr(x),
                        ..
                    } = a
                    {
                        each_query_mut(x, f);
                    }
                }
            }
            if let Some(x) = &mut fun.filter {
                each_query_mut(x, f);
            }
        }
        Expr::Tuple(xs) => xs.iter_mut().for_each(|x| each_query_mut(x, f)),
        Expr::Array(a) => a.elem.iter_mut().for_each(|x| each_query_mut(x, f)),
        _ => {}
    }
}

impl Plans<'_> {
    /// Whether an expression holds a subquery, each reading nothing of the query around it.
    pub(super) fn only_once(&self, e: &Expr) -> bool {
        let mut found = Vec::new();
        pipeline::subqueries_in(e, &mut found);
        !found.is_empty()
            && found.iter().all(|s| {
                self.plans
                    .get(&(*s as *const Expr))
                    .is_some_and(|p| p.around.is_none())
            })
    }
}
