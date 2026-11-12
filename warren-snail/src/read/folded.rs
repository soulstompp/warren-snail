//! A statement's constants folded before it is routed and before any row is read, as PostgreSQL
//! 18's planner folds them while it plans (eval_const_expressions, clauses.c): a call of an
//! immutable function returning no set, every argument a constant once folded, is evaluated once,
//! and its failure is the statement's, whether or not a row would have reached it. A STRICT one
//! given a NULL constant is NULL, uncalled; AND and OR stop at their first constant FALSE (TRUE),
//! CASE drops an arm whose condition folds FALSE or NULL and stops at one folding TRUE, COALESCE
//! stops at its first constant not NULL. Stable and volatile calls are left to the rows. A call of
//! one of the file's functions is called once where it is immutable and given constants, and is
//! otherwise read through its body where 18.6 inlines it (inline_function).
//!
//! The statement is read as `deparse::query` reads it, operators and functions by their OIDs, a
//! query level at a time as subquery_planner plans one. First its WITH queries: one read once and
//! not MATERIALIZED is read in place, each other one read is planned whole, one never read not at
//! all. Then its join tree as pull_up_sublinks and pull_up_subqueries leave it: an IN, = ANY or
//! EXISTS of the WHERE or of an ON that reads the query around it becomes a join after its FROM
//! list, and a simple FROM subquery is laid in its place, its outputs read only where the query
//! reads them. Then the target list (ORDER BY's and GROUP BY's hidden entries in their places),
//! RETURNING, each join's sides then its ON, WHERE, HAVING, the windows' frame offsets, OFFSET,
//! LIMIT, a UNION ALL's arms laid beside the query, a VALUES list's rows and, from release 18, the
//! GROUP BY expressions, which the target list and HAVING read from the grouping step; each clause's
//! subqueries planned after its constants, those the fold dropped never. Last the subqueries planned
//! on their own, each output nothing reads dropped first, none where it is the query's one relation
//! under a WHERE that folds FALSE, and a set operation's arms in their order.
//!
//! A constant is evaluated as the pipeline evaluates its printed text over no row; where the snail
//! refuses to evaluate it for a reason of its own the constant is left to the rows, as before. Where
//! that reading refuses the statement nothing is folded.
//!
//! Once it is folded, as its plan starts, a WHERE's condition that reads no column and calls
//! nothing volatile is evaluated once before any row (a gating Result's one-time filter), the
//! statement's own and then those of what its one FROM item starts; none under a LIMIT of none.

use super::pipeline::{self, Casings};
use crate::catalog::Catalog;
use crate::deparse::analyze::{BoolOp, Node};
use crate::deparse::cat::{self, Cat};
use crate::deparse::print::Printer;
use crate::deparse::query::{same, Bound, Cte, JoinKind, Jt, Query, RteKind, SetOp, SubKind};
use crate::Result;
use sqlparser::ast::{Expr, Statement, UnaryOperator};
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::rc::Rc;

/// A statement's constants folded: the first failure among them, in the planner's order, where
/// one fails; then its one-time conditions.
pub(super) fn folded(catalog: &Catalog, sql: &str) -> Result<()> {
    read(catalog, sql, true)
}

/// A query's one-time conditions alone, its constants folded with the statement it stands in: a
/// subquery reading nothing of the query around it, as a row first asks for it.
pub(super) fn gated(catalog: &Catalog, sql: &str) -> Result<()> {
    read(catalog, sql, false)
}

fn read(catalog: &Catalog, sql: &str, fold: bool) -> Result<()> {
    let bound = crate::grammar::bound(sql);
    let Ok(parsed) = super::image::parse_sql(bound.as_deref().unwrap_or(sql)) else {
        return Ok(());
    };
    let [st] = parsed.as_slice() else {
        return Ok(());
    };
    if !computes_a_constant(st) && !super::own::calls_in(catalog, st) && !types_a_literal(st) {
        return Ok(());
    }
    let cat = Cat::of(catalog);
    // A literal is read by its type's input, and a cast found, as the statement is analyzed: a
    // failure there is the statement's, as 18.6's parser gives it, before anything is folded. But
    // for a statement writing `@@@`, which sqlparser reads as `@@` and a prefix `@`.
    let q = match crate::deparse::query::statement(&cat, st) {
        Ok(q) => q,
        Err(e) if analyzed_refusal(&e) && !super::spelled::three_ats(sql) => return Err(e),
        Err(_) => return Ok(()),
    };
    // The file's functions held, so that a call of one is evaluated as a row's call is.
    let _own = super::own::holding(&catalog.functions, catalog.release);
    let folding = Folding {
        cat: &cat,
        release: catalog.release,
        casings: Casings::of_file(catalog.monetary, catalog.text_search),
        inlining: RefCell::new(Vec::new()),
        read: RefCell::new(HashSet::new()),
        places: Cell::new(0),
    };
    if fold {
        folding.level(&q, &Rc::new(Withs::default()), &[], &Reading::default())?;
    }
    // Then, as the plan starts and before any row, its one-time conditions.
    match folding.gate(&q, &[])? {
        Some(g) => folding.passes(&g),
        None => Ok(()),
    }
}

/// What 18.6 evaluates once as a level's plan starts, before it reads a row (a gating Result's
/// one-time filter): each condition of the level's WHERE, and of the ON of a join over its whole
/// FROM, that reads no column and calls nothing volatile, in order; where each is TRUE, what the
/// level below starts.
enum Gate {
    Level {
        conds: Vec<Node>,
        then: Option<Box<Gate>>,
    },
}

/// Whether a statement holds a quoted literal or a cast, which its analysis types.
fn types_a_literal(st: &Statement) -> bool {
    sqlparser::ast::visit_expressions(st, |e| match e {
        Expr::Cast { .. } => ControlFlow::Break(()),
        Expr::Value(v) if matches!(v.value, sqlparser::ast::Value::SingleQuotedString(_)) => {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    })
    .is_break()
}

/// A refusal of the statement's analysis that is 18.6's parser's: a literal its type's input
/// refuses (a data exception), or a cast of no path.
fn analyzed_refusal(e: &crate::Error) -> bool {
    e.sqlstate()
        .is_some_and(|s| s.starts_with("22") || s == "42846")
}

/// Whether a statement holds an expression computed of constants alone: none, and nothing is
/// folded.
fn computes_a_constant(st: &Statement) -> bool {
    let found = sqlparser::ast::visit_expressions(st, |e| match computed_of_constants(e) {
        true => ControlFlow::Break(()),
        false => ControlFlow::Continue(()),
    });
    found.is_break()
}

/// An expression that is no literal, reading no column, no parameter and no subquery.
fn computed_of_constants(e: &Expr) -> bool {
    let literal = |x: &Expr| {
        matches!(
            x,
            Expr::Value(_) | Expr::TypedString { .. } | Expr::Interval(_)
        )
    };
    match e {
        _ if literal(e) => return false,
        Expr::Identifier(_) | Expr::CompoundIdentifier(_) | Expr::Nested(_) => return false,
        Expr::UnaryOp {
            op: UnaryOperator::Minus | UnaryOperator::Plus,
            expr,
        } if literal(expr) => return false,
        Expr::Cast { expr, .. } if literal(expr) => return false,
        // A call of no argument is never folded to a failure.
        Expr::Function(f) if !matches!(&f.args, sqlparser::ast::FunctionArguments::List(l) if !l.args.is_empty()) =>
        {
            return false;
        }
        _ => {}
    }
    let reads = sqlparser::ast::visit_expressions(e, |x| match x {
        Expr::Identifier(_)
        | Expr::CompoundIdentifier(_)
        | Expr::Subquery(_)
        | Expr::Exists { .. }
        | Expr::InSubquery { .. }
        | Expr::Wildcard(_)
        | Expr::QualifiedWildcard(..) => ControlFlow::Break(()),
        Expr::Value(v) if matches!(v.value, sqlparser::ast::Value::Placeholder(_)) => {
            ControlFlow::Break(())
        }
        _ => ControlFlow::Continue(()),
    });
    !reads.is_break()
}

/// A subquery met in an expression, planned once the clause it stands in is folded: the SubLink,
/// the places around it, innermost last, and the WITH queries it reads through.
#[derive(Clone)]
struct Pending<'r> {
    link: Node,
    around: Vec<usize>,
    withs: Rc<Withs<'r>>,
}

/// What an expression folds to: a constant, or an expression left to the rows, with the
/// subqueries still in it.
#[derive(Clone)]
enum Shape<'r> {
    Const(Node),
    Varies(Vec<Pending<'r>>),
}

impl Shape<'_> {
    fn constant(&self) -> Option<&Node> {
        match self {
            Shape::Const(n) => Some(n),
            Shape::Varies(_) => None,
        }
    }

    fn is_null(&self) -> bool {
        matches!(self, Shape::Const(Node::Const { text: None, .. }))
    }

    /// A boolean constant's truth: None for NULL.
    fn truth(&self) -> Option<Option<bool>> {
        match self {
            Shape::Const(Node::Const { text, .. }) => Some(text.as_deref().map(|t| t == "t")),
            _ => None,
        }
    }
}

/// Left to the rows, with every subquery its parts hold.
fn varies<'r>(parts: impl IntoIterator<Item = Shape<'r>>) -> Shape<'r> {
    let mut held = Vec::new();
    for p in parts {
        if let Shape::Varies(s) = p {
            held.extend(s);
        }
    }
    Shape::Varies(held)
}

fn boolean<'r>(b: Option<bool>) -> Shape<'r> {
    Shape::Const(Node::Const {
        ty: cat::BOOL,
        typmod: -1,
        text: b.map(|b| if b { "t" } else { "f" }.to_string()),
    })
}

fn null_of<'r>(n: &Node) -> Shape<'r> {
    Shape::Const(Node::Const {
        ty: n.ty(),
        typmod: n.typmod(),
        text: None,
    })
}

/// The WITH queries a level reads through: its own, then those around it, each read in place
/// (18.6 inlines it) or planned once on its own.
#[derive(Default)]
struct Withs<'r> {
    outer: Option<Rc<Withs<'r>>>,
    ctes: Vec<(&'r Cte, bool)>,
}

/// The WITH query a name reads, whether it is read in place, and the WITH queries it was written
/// among.
fn named_with<'r>(w: &Rc<Withs<'r>>, name: &str) -> Option<(&'r Cte, bool, Rc<Withs<'r>>)> {
    let mut at = Some(w.clone());
    while let Some(x) = at {
        if let Some((c, inline)) = x.ctes.iter().find(|(c, _)| c.name == name) {
            return Some((*c, *inline, x.clone()));
        }
        at = x.outer.clone();
    }
    None
}

/// A query level, or a subquery pulled up into one, as pull_up_subqueries leaves it.
struct Laid<'r> {
    q: &'r Query,
    withs: Rc<Withs<'r>>,
    /// A number of its own, by which the outputs read of what stays in it are marked.
    place: usize,
    /// Each subquery pulled up into it, by its place in the range table (from 1); an INSERT's
    /// SELECT at 0.
    pulled: HashMap<u32, Rc<Laid<'r>>>,
    tree: Tree<'r>,
}

/// A join tree as pull_up_sublinks and pull_up_subqueries leave it.
enum Tree<'r> {
    /// A table, a WITH query planned on its own, a VALUES list, or a subquery planned after.
    Leaf,
    /// A subquery pulled up in its place.
    Pulled(Rc<Laid<'r>>),
    /// A FROM list: its items, what is left of its WHERE, then the joins its sublinks became.
    From {
        items: Vec<Tree<'r>>,
        quals: Vec<&'r Node>,
        semis: Vec<Semi<'r>>,
    },
    /// A join: its sides, what is left of its ON, and the joins its sublinks became, laid where
    /// 18.6 lays them.
    Join {
        kind: JoinKind,
        left: Box<Tree<'r>>,
        right: Box<Tree<'r>>,
        quals: Vec<&'r Node>,
        semis: Vec<Semi<'r>>,
    },
}

/// A sublink pulled up as a join.
enum Semi<'r> {
    /// IN or = ANY: its left side, then the subquery's outputs; the subquery pulled up, or none
    /// where it is planned after.
    Any {
        left: &'r Node,
        sub: Option<Rc<Laid<'r>>>,
    },
    /// EXISTS or NOT EXISTS: the subquery's FROM, then its WHERE.
    Exists(Rc<Laid<'r>>),
}

/// A subquery planned on its own, after the expressions of the level it stands in.
struct Later<'r> {
    q: &'r Query,
    withs: Rc<Withs<'r>>,
    /// Where its outputs are read: the place it stands in and its place in that range table; none
    /// where each is read (a set operation's arm, an IN's subquery).
    read: Option<(usize, u32)>,
    /// The place it stands in, which its references to the query around it read.
    around: usize,
    /// Its turn: the level's own range table first, in its order, then the rest as they were met.
    turn: (u8, usize),
}

/// What laying a level out gathers for after its own expressions.
#[derive(Default)]
struct Laying<'r> {
    /// A UNION ALL's arms laid out beside the query, whose outputs are folded after LIMIT.
    beside: Vec<Rc<Laid<'r>>>,
    /// The VALUES lists of the level and of what it pulled up.
    values: Vec<&'r [Vec<Node>]>,
    later: Vec<Later<'r>>,
    /// The relations 18.6 plans the level over.
    relations: usize,
    met: usize,
}

/// How a level is read: whole, as 18.6 reads an EXISTS's query, or as it reads a subquery it
/// plans on its own.
#[derive(Default)]
struct Reading {
    /// An EXISTS's query, its select list, grouping, windows and LIMIT thrown away.
    exists: bool,
    /// The outputs (from 0) no query above reads, each dropped before the level is planned.
    unread: Vec<usize>,
    /// A recursive WITH query's own, whose UNION ALL is not laid out beside it.
    recursive: bool,
}

/// Where an expression is folded: the constant a simple CASE's test folded to, which its WHEN
/// clauses compare; the GROUP BY expressions its query's grouping step computes, which the target
/// list and HAVING read as that step's columns; the level it reads its columns in and the places
/// around it; and a call's arguments, where its function's body is read in the call's place.
#[derive(Clone, Copy)]
struct At<'a, 'r> {
    case: Option<&'a Node>,
    grouped: &'a [&'a Node],
    scope: &'a Laid<'r>,
    around: &'a [usize],
    args: Option<&'a Args<'a, 'r>>,
}

/// A call's arguments as its function's body reads them: each a constant it folded to, or the
/// argument as written, read where the call stands.
struct Args<'a, 'r> {
    given: Vec<Given<'a>>,
    at: At<'a, 'r>,
}

enum Given<'a> {
    Const(Node),
    Expr(&'a Node),
}

struct Folding<'c, 'a> {
    cat: &'c Cat<'a>,
    release: u8,
    casings: Casings,
    /// The file's functions being read through their bodies, innermost last.
    inlining: RefCell<Vec<u32>>,
    /// Each column read of what stays in a level: its place, its range table entry, the column
    /// (0 the whole row).
    read: RefCell<HashSet<(usize, u32, i16)>>,
    places: Cell<usize>,
}

impl<'c, 'a> Folding<'c, 'a> {
    fn place(&self) -> usize {
        let p = self.places.get() + 1;
        self.places.set(p);
        p
    }

    /// One query level, in subquery_planner's order.
    fn level<'r>(
        &self,
        q: &'r Query,
        outer: &Rc<Withs<'r>>,
        around: &[usize],
        reading: &Reading,
    ) -> Result<()> {
        let withs = self.withs(q, outer, around)?;
        let mut laying = Laying::default();
        let laid = self.lay(q, &withs, &mut laying, false, true)?;
        if q.setop.is_some() {
            self.arms(q, &withs, &mut laying, reading.recursive, laid.place)?;
        }
        // From release 18 the grouping step computes the GROUP BY expressions, after LIMIT.
        let grouped: Vec<&Node> = match self.release >= 18 && !reading.exists {
            true => q.group.iter().map(|s| &s.expr).collect(),
            false => Vec::new(),
        };
        let plain = At {
            case: None,
            grouped: &[],
            scope: &laid,
            around,
            args: None,
        };
        let reads_grouped = At {
            grouped: &grouped,
            ..plain
        };
        // A write's values: an UPDATE's SET is its target list, an INSERT's rows and its ON
        // CONFLICT beside them; its RETURNING after.
        if !reading.exists {
            let assigned: Vec<(&Node, At)> = q.assigned.iter().map(|n| (n, plain)).collect();
            self.clause(&assigned)?;
            let mut output = 0;
            let mut targets = Vec::new();
            for t in &q.targets {
                if !t.junk {
                    output += 1;
                    if reading.unread.contains(&(output - 1)) {
                        continue;
                    }
                }
                targets.push((&t.expr, reads_grouped));
            }
            self.clause(&targets)?;
        }
        let falsified = self.walk(&laid.tree, &laid, around, true)?;
        if let Some(h) = &q.having {
            self.clause(&[(h, reads_grouped)])?;
        }
        if !reading.exists {
            for w in &q.windows {
                if let Some(f) = &w.frame {
                    for b in std::iter::once(&f.start).chain(&f.end) {
                        if let Bound::Preceding(n) | Bound::Following(n) = b {
                            self.clause(&[(n, plain)])?;
                        }
                    }
                }
            }
        }
        if let Some(o) = &q.offset {
            self.clause(&[(o, plain)])?;
        }
        if let (Some(l), false) = (&q.limit, reading.exists) {
            self.clause(&[(l, plain)])?;
        }
        let inner: Vec<usize> = around.iter().copied().chain([laid.place]).collect();
        let beside: Vec<(&Node, At)> = laying
            .beside
            .iter()
            .flat_map(|arm| {
                let at = At {
                    scope: arm,
                    around: &inner,
                    ..plain
                };
                arm.q.output().map(move |t| (&t.expr, at))
            })
            .collect();
        self.clause(&beside)?;
        for rows in &laying.values {
            let cells: Vec<(&Node, At)> = rows.iter().flatten().map(|n| (n, plain)).collect();
            self.clause(&cells)?;
        }
        if !grouped.is_empty() {
            let exprs: Vec<(&Node, At)> = grouped.iter().map(|n| (*n, plain)).collect();
            self.clause(&exprs)?;
        }
        // What 18.6 plans on its own, after: none where the level's one relation is under a
        // condition that folded FALSE or NULL (relation_excluded_by_constraints).
        let lone = laying.relations == 1 && falsified;
        let mut later = laying.later;
        later.sort_by_key(|l| l.turn);
        for l in &later {
            self.mark_read(l.q, 1, l.around);
        }
        for l in &later {
            if lone && l.read.is_some() {
                continue;
            }
            let unread = self.unread(l);
            let inner: Vec<usize> = around.iter().copied().chain([l.around]).collect();
            self.level(
                l.q,
                &l.withs,
                &inner,
                &Reading {
                    unread,
                    ..Reading::default()
                },
            )?;
        }
        Ok(())
    }

    /// A level's WITH queries (SS_process_ctes): each read once and not MATERIALIZED, or NOT
    /// MATERIALIZED, recursing into nothing and calling nothing volatile, is read in place; each
    /// other one read is planned whole, in order; one never read is never planned.
    fn withs<'r>(
        &self,
        q: &'r Query,
        outer: &Rc<Withs<'r>>,
        around: &[usize],
    ) -> Result<Rc<Withs<'r>>> {
        if q.ctes.is_empty() {
            return Ok(outer.clone());
        }
        let decided: Vec<(&Cte, bool, usize, bool)> = q
            .ctes
            .iter()
            .map(|c| {
                let reads = cte_reads(q, &c.name, Some(c));
                let recursive = q.recursive && cte_reads(&c.query, &c.name, None) > 0;
                let inline = reads > 0
                    && !recursive
                    && c.materialized != Some(true)
                    && (reads == 1 || c.materialized == Some(false))
                    && !self.volatile_query(&c.query);
                (c, inline, reads, recursive)
            })
            .collect();
        let withs = Rc::new(Withs {
            outer: Some(outer.clone()),
            ctes: decided.iter().map(|d| (d.0, d.1)).collect(),
        });
        // The query the WITH stands in, which no WITH query's own reads may name.
        let inner: Vec<usize> = around.iter().copied().chain([self.place()]).collect();
        for (c, inline, reads, recursive) in &decided {
            if !inline && *reads > 0 {
                let reading = Reading {
                    recursive: *recursive,
                    ..Reading::default()
                };
                self.level(&c.query, &withs, &inner, &reading)?;
            }
        }
        Ok(withs)
    }

    /// A level, or a subquery pulled up into one, laid out: its FROM list's subqueries pulled up
    /// where they are simple, its sublinks pulled up as joins.
    fn lay<'r>(
        &self,
        q: &'r Query,
        withs: &Rc<Withs<'r>>,
        laying: &mut Laying<'r>,
        outer: bool,
        own: bool,
    ) -> Result<Rc<Laid<'r>>> {
        let place = self.place();
        let mut pulled = HashMap::new();
        let mut items = Vec::new();
        let mut rels = HashSet::new();
        for jt in &q.from {
            rels.extend(jt_rels(jt));
            items.push(self.jt(q, place, jt, withs, &mut pulled, laying, outer, own)?);
        }
        // An INSERT's SELECT is its one FROM item, read through the row's values.
        let select = match q.setop {
            None => q.rtable.iter().find_map(|r| match &r.kind {
                RteKind::Subquery(s) if !r.in_from => Some(&**s),
                _ => None,
            }),
            Some(_) => None,
        };
        if let Some(s) = select {
            rels.insert(0);
            items.push(self.subquery(
                place,
                0,
                s,
                withs.clone(),
                &mut pulled,
                laying,
                outer,
                (0, 0),
            )?);
        }
        let (quals, semis) = self.joined_sublinks(place, q.quals.as_ref(), &rels, withs, laying)?;
        laying
            .values
            .extend(q.rtable.iter().filter_map(|r| match &r.kind {
                RteKind::Values { rows } => Some(rows.as_slice()),
                _ => None,
            }));
        Ok(Rc::new(Laid {
            q,
            withs: withs.clone(),
            place,
            pulled,
            tree: Tree::From {
                items,
                quals,
                semis,
            },
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn jt<'r>(
        &self,
        q: &'r Query,
        place: usize,
        jt: &'r Jt,
        withs: &Rc<Withs<'r>>,
        pulled: &mut HashMap<u32, Rc<Laid<'r>>>,
        laying: &mut Laying<'r>,
        outer: bool,
        own: bool,
    ) -> Result<Tree<'r>> {
        match jt {
            Jt::Ref(i) => {
                let rte = &q.rtable[*i - 1];
                let turn = match own {
                    true => (0, *i),
                    false => (1, 0),
                };
                match &rte.kind {
                    RteKind::Subquery(s) => self.subquery(
                        place,
                        *i as u32,
                        s,
                        withs.clone(),
                        pulled,
                        laying,
                        outer,
                        turn,
                    ),
                    RteKind::Cte { name } => match named_with(withs, name) {
                        Some((c, true, written)) => self.subquery(
                            place, *i as u32, &c.query, written, pulled, laying, outer, turn,
                        ),
                        _ => {
                            laying.relations += 1;
                            Ok(Tree::Leaf)
                        }
                    },
                    _ => {
                        laying.relations += 1;
                        Ok(Tree::Leaf)
                    }
                }
            }
            Jt::Join(j) => {
                let below = outer || j.kind != JoinKind::Inner;
                let left = self.jt(q, place, &j.left, withs, pulled, laying, below, own)?;
                let right = self.jt(q, place, &j.right, withs, pulled, laying, below, own)?;
                let (l, r) = (jt_rels(&j.left), jt_rels(&j.right));
                let available: HashSet<u32> = match j.kind {
                    JoinKind::Inner => l.union(&r).copied().collect(),
                    JoinKind::Left => r,
                    JoinKind::Right => l,
                    JoinKind::Full => HashSet::new(),
                };
                let (quals, semis) = match j.kind {
                    JoinKind::Full => (conjuncts(j.quals.as_ref()), Vec::new()),
                    _ => {
                        self.joined_sublinks(place, j.quals.as_ref(), &available, withs, laying)?
                    }
                };
                Ok(Tree::Join {
                    kind: j.kind,
                    left: Box::new(left),
                    right: Box::new(right),
                    quals,
                    semis,
                })
            }
        }
    }

    /// A FROM subquery: pulled up where it is simple; a simple UNION ALL's arms laid beside the
    /// query or planned after; any other planned after.
    #[allow(clippy::too_many_arguments)]
    fn subquery<'r>(
        &self,
        place: usize,
        rel: u32,
        s: &'r Query,
        withs: Rc<Withs<'r>>,
        pulled: &mut HashMap<u32, Rc<Laid<'r>>>,
        laying: &mut Laying<'r>,
        outer: bool,
        turn: (u8, usize),
    ) -> Result<Tree<'r>> {
        if self.simple(s, outer) {
            let laid = self.lay(s, &withs, laying, outer, false)?;
            pulled.insert(rel, laid.clone());
            return Ok(Tree::Pulled(laid));
        }
        laying.relations += 1;
        laying.met += 1;
        let turn = match turn.0 {
            0 => turn,
            _ => (1, laying.met),
        };
        if self.simple_union_all(s) {
            let mut leaves = Vec::new();
            setop_leaves(s.setop.as_ref().expect("a set operation"), &mut leaves);
            for i in leaves {
                let RteKind::Subquery(arm) = &s.rtable[i - 1].kind else {
                    continue;
                };
                if self.simple(arm, outer) && safe_append(arm) {
                    let relations = laying.relations;
                    let laid = self.lay(arm, &withs, laying, outer, false)?;
                    laying.relations = relations;
                    laying.beside.push(laid);
                } else {
                    laying.later.push(Later {
                        q: arm,
                        withs: withs.clone(),
                        read: Some((place, rel)),
                        around: place,
                        turn,
                    });
                }
            }
            return Ok(Tree::Leaf);
        }
        laying.later.push(Later {
            q: s,
            withs,
            read: Some((place, rel)),
            around: place,
            turn,
        });
        Ok(Tree::Leaf)
    }

    /// A top-level set operation's arms: a UNION ALL of arms of the query's own types laid out
    /// beside the query (flatten_simple_union_all), each simple arm read with it, each other
    /// planned after; any other operation's arms planned after, in order.
    fn arms<'r>(
        &self,
        q: &'r Query,
        withs: &Rc<Withs<'r>>,
        laying: &mut Laying<'r>,
        recursive: bool,
        place: usize,
    ) -> Result<()> {
        let Some(tree) = &q.setop else {
            return Ok(());
        };
        let mut leaves = Vec::new();
        setop_leaves(tree, &mut leaves);
        let types: Vec<u32> = q.output().map(|t| t.expr.ty()).collect();
        let arm = |i: usize| match &q.rtable[i - 1].kind {
            RteKind::Subquery(s) => Some(&**s),
            _ => None,
        };
        let flat = !recursive
            && union_all(tree)
            && leaves.iter().all(|&i| {
                arm(i).is_some_and(|s| s.output().map(|t| t.expr.ty()).eq(types.iter().copied()))
            });
        for (k, &i) in leaves.iter().enumerate() {
            let Some(s) = arm(i) else {
                continue;
            };
            if flat && self.simple(s, false) && safe_append(s) {
                let relations = laying.relations;
                let laid = self.lay(s, withs, laying, false, false)?;
                laying.relations = relations;
                laying.beside.push(laid);
            } else {
                laying.later.push(Later {
                    q: s,
                    withs: withs.clone(),
                    read: None,
                    around: place,
                    turn: (2, k),
                });
            }
        }
        Ok(())
    }

    /// A WHERE's or an ON's conditions with each IN, = ANY, EXISTS and NOT EXISTS that 18.6 joins
    /// to the query taken out (pull_up_sublinks): what is left, and the joins, in order.
    fn joined_sublinks<'r>(
        &self,
        place: usize,
        quals: Option<&'r Node>,
        available: &HashSet<u32>,
        withs: &Rc<Withs<'r>>,
        laying: &mut Laying<'r>,
    ) -> Result<(Vec<&'r Node>, Vec<Semi<'r>>)> {
        let mut kept = Vec::new();
        let mut semis = Vec::new();
        for c in conjuncts(quals) {
            let exists = match c {
                Node::SubLink {
                    kind: SubKind::Exists,
                    query,
                    ..
                } => Some(&**query),
                Node::Bool {
                    op: BoolOp::Not,
                    args,
                } => match args.as_slice() {
                    [Node::SubLink {
                        kind: SubKind::Exists,
                        query,
                        ..
                    }] => Some(&**query),
                    _ => None,
                },
                _ => None,
            };
            match c {
                Node::SubLink {
                    kind: SubKind::Any,
                    test: Some((left, _, _)),
                    query,
                    ..
                } if self.joins_any(left, query, available) => {
                    let sub = match self.simple(query, false) {
                        true => Some(self.lay(query, withs, laying, false, false)?),
                        false => {
                            laying.relations += 1;
                            laying.met += 1;
                            laying.later.push(Later {
                                q: query,
                                withs: withs.clone(),
                                read: None,
                                around: place,
                                turn: (1, laying.met),
                            });
                            None
                        }
                    };
                    semis.push(Semi::Any { left, sub });
                }
                _ => match exists {
                    Some(s) if self.joins_exists(s, available, withs, place)? => {
                        semis.push(Semi::Exists(self.lay(s, withs, laying, false, false)?));
                    }
                    _ => kept.push(c),
                },
            }
        }
        Ok((kept, semis))
    }

    /// Whether 18.6 joins an IN or = ANY to the query (convert_ANY_sublink_to_join): its left side
    /// reads the query's relations it may, and only those, through nothing volatile; its subquery
    /// reads none of the query's others.
    fn joins_any(&self, left: &Node, query: &Query, available: &HashSet<u32>) -> bool {
        let mut outer = HashSet::new();
        query_rels_at(query, 1, &mut outer);
        let mut upper = HashSet::new();
        node_rels_at(left, 0, &mut upper);
        outer.is_subset(available)
            && !upper.is_empty()
            && upper.is_subset(available)
            && !self.volatile(left)
    }

    /// Whether 18.6 joins an EXISTS to the query (convert_EXISTS_sublink_to_join): its query holds
    /// no WITH and its select list is thrown away; its WHERE reads the query around it, through
    /// nothing volatile and only the relations it may, and the rest of it reads nothing around it.
    fn joins_exists<'r>(
        &self,
        s: &'r Query,
        available: &HashSet<u32>,
        withs: &Rc<Withs<'r>>,
        place: usize,
    ) -> Result<bool> {
        if !s.ctes.is_empty() || !self.thrown_away(s, withs, &[place])? {
            return Ok(false);
        }
        let Some(w) = &s.quals else {
            return Ok(false);
        };
        let mut beside = HashSet::new();
        for jt in &s.from {
            jt_quals_rels_at(jt, 1, &mut beside);
        }
        for r in &s.rtable {
            match &r.kind {
                RteKind::Subquery(sub) => query_rels_at(sub, 2, &mut beside),
                RteKind::Values { rows } => rows
                    .iter()
                    .flatten()
                    .for_each(|n| node_rels_at(n, 1, &mut beside)),
                _ => {}
            }
        }
        if !beside.is_empty() {
            return Ok(false);
        }
        let mut upper = HashSet::new();
        node_rels_at(w, 1, &mut upper);
        Ok(!upper.is_empty() && upper.is_subset(available) && !self.volatile(w))
    }

    /// Whether 18.6 throws an EXISTS's select list away (simplify_EXISTS_query): nothing in its
    /// query counts rows, and a LIMIT, folded, is NULL or more than none.
    fn thrown_away<'r>(
        &self,
        s: &'r Query,
        withs: &Rc<Withs<'r>>,
        around: &[usize],
    ) -> Result<bool> {
        if s.setop.is_some()
            || s.having.is_some()
            || s.offset.is_some()
            || !s.grouping_sets.is_empty()
            || !s.assigned.is_empty()
            || self.counts_rows(s)
        {
            return Ok(false);
        }
        let Some(l) = &s.limit else {
            return Ok(true);
        };
        let bare = Laid {
            q: s,
            withs: withs.clone(),
            place: self.place(),
            pulled: HashMap::new(),
            tree: Tree::Leaf,
        };
        let at = At {
            case: None,
            grouped: &[],
            scope: &bare,
            around,
            args: None,
        };
        Ok(match self.node(l, &at)? {
            Shape::Const(Node::Const { text: None, .. }) => true,
            Shape::Const(Node::Const { text: Some(t), .. }) => {
                t.parse::<i64>().is_ok_and(|n| n > 0)
            }
            _ => false,
        })
    }

    /// Whether 18.6 pulls a subquery up into the query around it (is_simple_subquery).
    fn simple(&self, s: &Query, outer: bool) -> bool {
        let mut around = HashSet::new();
        if outer {
            query_rels_at(s, 1, &mut around);
        }
        s.setop.is_none()
            && s.assigned.is_empty()
            && s.group.is_empty()
            && s.grouping_sets.is_empty()
            && s.having.is_none()
            && s.sort.is_empty()
            && !s.distinct
            && s.distinct_on.is_empty()
            && s.limit.is_none()
            && s.offset.is_none()
            && s.ctes.is_empty()
            && !self.counts_rows(s)
            && around.is_empty()
            && !s.targets.iter().any(|t| self.volatile(&t.expr))
    }

    /// Whether 18.6 lays a FROM subquery's UNION ALL out as arms beside the query
    /// (is_simple_union_all).
    fn simple_union_all(&self, s: &Query) -> bool {
        let Some(tree) = &s.setop else {
            return false;
        };
        let mut leaves = Vec::new();
        setop_leaves(tree, &mut leaves);
        let types: Vec<u32> = s.output().map(|t| t.expr.ty()).collect();
        union_all(tree)
            && s.sort.is_empty()
            && s.limit.is_none()
            && s.offset.is_none()
            && s.ctes.is_empty()
            && leaves.iter().all(|&i| match &s.rtable[i - 1].kind {
                RteKind::Subquery(arm) => {
                    arm.output().map(|t| t.expr.ty()).eq(types.iter().copied())
                }
                _ => false,
            })
    }

    /// Whether a query's own expressions aggregate, window or return sets.
    fn counts_rows(&self, s: &Query) -> bool {
        !s.windows.is_empty()
            || s.targets.iter().map(|t| &t.expr).chain(&s.having).any(|n| {
                any_node(n, &mut |x| match x {
                    Node::Agg { .. } | Node::Window { .. } => true,
                    Node::Func { oid, .. } => self.cat.proc(*oid).is_some_and(|p| p.retset),
                    _ => false,
                })
            })
    }

    /// The join tree folded in subquery_planner's order: whether a condition 18.6 lays over the
    /// whole query folded FALSE or NULL.
    fn walk<'r>(
        &self,
        t: &Tree<'r>,
        scope: &Laid<'r>,
        around: &[usize],
        top: bool,
    ) -> Result<bool> {
        let inner: Vec<usize> = around.iter().copied().chain([scope.place]).collect();
        Ok(match t {
            Tree::Leaf => false,
            Tree::Pulled(sub) => self.walk(&sub.tree, sub, &inner, top)?,
            Tree::From {
                items,
                quals,
                semis,
            } => {
                let mut falsified = false;
                for i in items {
                    falsified |= self.walk(i, scope, around, top)?;
                }
                falsified |= self.quals(quals, scope, around, top)?;
                for s in semis {
                    self.semi(s, scope, around)?;
                }
                falsified
            }
            Tree::Join {
                kind,
                left,
                right,
                quals,
                semis,
            } => match kind {
                JoinKind::Inner => {
                    let mut falsified = self.walk(left, scope, around, top)?;
                    falsified |= self.walk(right, scope, around, top)?;
                    falsified |= self.quals(quals, scope, around, top)?;
                    for s in semis {
                        self.semi(s, scope, around)?;
                    }
                    falsified
                }
                JoinKind::Left => {
                    let falsified = self.walk(left, scope, around, top)?;
                    self.walk(right, scope, around, false)?;
                    for s in semis {
                        self.semi(s, scope, around)?;
                    }
                    self.quals(quals, scope, around, false)?;
                    falsified
                }
                JoinKind::Right => {
                    self.walk(left, scope, around, false)?;
                    for s in semis {
                        self.semi(s, scope, around)?;
                    }
                    let falsified = self.walk(right, scope, around, top)?;
                    self.quals(quals, scope, around, false)?;
                    falsified
                }
                JoinKind::Full => {
                    self.walk(left, scope, around, false)?;
                    self.walk(right, scope, around, false)?;
                    self.quals(quals, scope, around, false)?;
                    false
                }
            },
        })
    }

    /// A WHERE's or an ON's conditions folded as one AND, then its subqueries planned: whether
    /// they folded FALSE or NULL where the condition is over the whole query.
    fn quals<'r>(
        &self,
        quals: &[&'r Node],
        scope: &Laid<'r>,
        around: &[usize],
        top: bool,
    ) -> Result<bool> {
        if quals.is_empty() {
            return Ok(false);
        }
        let at = At {
            case: None,
            grouped: &[],
            scope,
            around,
            args: None,
        };
        let shape = match quals {
            [one] => self.node(one, &at)?,
            many => self.and_or(many, false, &at)?,
        };
        self.planned(std::slice::from_ref(&shape))?;
        Ok(top && matches!(shape.truth(), Some(Some(false) | None)))
    }

    /// A sublink pulled up as a join: its subquery's FROM and WHERE, then for an IN its left side
    /// and the subquery's outputs.
    fn semi<'r>(&self, s: &Semi<'r>, scope: &Laid<'r>, around: &[usize]) -> Result<()> {
        let inner: Vec<usize> = around.iter().copied().chain([scope.place]).collect();
        match s {
            Semi::Any { left, sub } => {
                if let Some(sub) = sub {
                    self.walk(&sub.tree, sub, &inner, false)?;
                }
                let at = At {
                    case: None,
                    grouped: &[],
                    scope,
                    around,
                    args: None,
                };
                let mut shapes = vec![self.node(left, &at)?];
                if let Some(sub) = sub {
                    let sub_at = At {
                        scope: sub,
                        around: &inner,
                        ..at
                    };
                    for t in sub.q.output() {
                        shapes.push(self.node(&t.expr, &sub_at)?);
                    }
                }
                self.planned(&shapes)
            }
            Semi::Exists(sub) => self.walk(&sub.tree, sub, &inner, false).map(|_| ()),
        }
    }

    /// A clause folded whole, then each subquery left in it planned, in order.
    fn clause<'r>(&self, nodes: &[(&Node, At<'_, 'r>)]) -> Result<Vec<Shape<'r>>> {
        let shapes = nodes
            .iter()
            .map(|(n, at)| self.node(n, at))
            .collect::<Result<Vec<_>>>()?;
        self.planned(&shapes)?;
        Ok(shapes)
    }

    fn planned(&self, shapes: &[Shape<'_>]) -> Result<()> {
        for s in shapes {
            if let Shape::Varies(held) = s {
                for p in held {
                    self.sublink(p)?;
                }
            }
        }
        Ok(())
    }

    /// A subquery of an expression planned as its own level; an EXISTS's with its select list
    /// thrown away where 18.6 throws it away.
    fn sublink(&self, p: &Pending<'_>) -> Result<()> {
        let Node::SubLink { kind, query, .. } = &p.link else {
            return Ok(());
        };
        let exists = *kind == SubKind::Exists && self.thrown_away(query, &p.withs, &p.around)?;
        let reading = Reading {
            exists,
            ..Reading::default()
        };
        self.level(query, &p.withs, &p.around, &reading)
    }

    /// The outputs of a subquery planned on its own that 18.6 drops before it plans it
    /// (remove_unused_subquery_outputs): each the query above never reads, sorts or groups by,
    /// returning no set and calling nothing volatile; none of a set operation's, a DISTINCT's, or
    /// one whose whole row is read.
    fn unread(&self, l: &Later<'_>) -> Vec<usize> {
        let Some((place, rel)) = l.read else {
            return Vec::new();
        };
        let s = l.q;
        let read = self.read.borrow();
        if s.setop.is_some()
            || (s.distinct && s.distinct_on.is_empty())
            || read.contains(&(place, rel, 0))
        {
            return Vec::new();
        }
        let sorted: HashSet<usize> = s
            .sort
            .iter()
            .chain(&s.group)
            .chain(&s.distinct_on)
            .chain(
                s.windows
                    .iter()
                    .flat_map(|w| w.partition.iter().chain(&w.order)),
            )
            .map(|x| x.resno)
            .collect();
        s.output()
            .enumerate()
            .filter(|(i, t)| {
                let resno = i + 1;
                !read.contains(&(place, rel, resno as i16))
                    && !sorted.contains(&resno)
                    && !any_node(&t.expr, &mut |x| {
                        matches!(x, Node::Func { oid, .. } if self.cat.proc(*oid).is_some_and(|p| p.retset))
                    })
                    && !self.volatile(&t.expr)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Each column a query `depth` levels below a place reads of it, marked read.
    fn mark_read(&self, q: &Query, depth: u32, place: usize) {
        let mut cols = HashSet::new();
        query_columns_at(q, depth, &mut cols);
        let mut read = self.read.borrow_mut();
        for (rel, attno) in cols {
            read.insert((place, rel, attno));
        }
    }

    /// Whether an expression calls anything volatile, its subqueries' too.
    fn volatile(&self, n: &Node) -> bool {
        let v = |oid: u32| self.cat.proc(oid).is_some_and(|p| p.volatility == 'v');
        any_node(n, &mut |x| match x {
            Node::Func { oid, .. } | Node::Agg { oid, .. } | Node::Window { oid, .. } => v(*oid),
            Node::Op { oid, .. } | Node::AnyAll { oid, .. } => {
                self.cat.oper(*oid).is_some_and(|o| v(o.code))
            }
            Node::Io { arg, ty, .. } => {
                v(self.cat.ty(arg.ty()).map_or(0, |t| t.output))
                    || v(self.cat.ty(*ty).map_or(0, |t| t.input))
            }
            Node::SubLink { query, .. } => self.volatile_query(query),
            _ => false,
        })
    }

    fn volatile_query(&self, q: &Query) -> bool {
        roots(q).into_iter().any(|n| self.volatile(n))
            || q.rtable
                .iter()
                .any(|r| matches!(&r.kind, RteKind::Subquery(s) if self.volatile_query(s)))
            || q.ctes.iter().any(|c| self.volatile_query(&c.query))
    }

    /// A level's one-time conditions, where the plan is sure to start it before any row: none
    /// where its WHERE folded FALSE or NULL, and none from the first that holds a subquery on,
    /// whose value the snail gives as a row asks for it. Below it, its one FROM item where that is
    /// a subquery or a WITH query, an INSERT's SELECT, or a set operation's first arm.
    fn gate(&self, q: &Query, ctes: &[&Cte]) -> Result<Option<Gate>> {
        let laid = Laid {
            q,
            withs: Rc::new(Withs::default()),
            place: 0,
            pulled: HashMap::new(),
            tree: Tree::Leaf,
        };
        let at = At {
            case: None,
            grouped: &[],
            scope: &laid,
            around: &[],
            args: None,
        };
        if !self.starts(q, &at)? {
            return Ok(None);
        }
        let mut visible = ctes.to_vec();
        if !q.recursive {
            visible.extend(&q.ctes);
        }
        if let Some(t) = &q.setop {
            let mut leaves = Vec::new();
            setop_leaves(t, &mut leaves);
            return match leaves.first().map(|&i| &q.rtable[i - 1].kind) {
                Some(RteKind::Subquery(s)) => self.gate(s, &visible),
                _ => Ok(None),
            };
        }
        // A join's ON over the whole FROM is read with the WHERE, before it.
        let on = match q.from.as_slice() {
            [Jt::Join(j)] if j.kind == JoinKind::Inner => j.quals.as_ref(),
            _ => None,
        };
        for w in on.into_iter().chain(&q.quals) {
            if matches!(self.node(w, &at)?.truth(), Some(Some(false) | None)) {
                return Ok(None);
            }
        }
        let mut conds = Vec::new();
        for c in conjuncts(on).into_iter().chain(conjuncts(q.quals.as_ref())) {
            let mut read = HashSet::new();
            node_columns_at(c, 0, &mut read);
            if !read.is_empty() || self.volatile(c) {
                continue;
            }
            if any_node(c, &mut |x| matches!(x, Node::SubLink { .. })) {
                return Ok(Some(Gate::Level { conds, then: None }));
            }
            if self.node(c, &at)?.constant().is_none() {
                conds.push(c.clone());
            }
        }
        let one = match q.from.as_slice() {
            [Jt::Ref(i)] => q.rtable.get(*i - 1),
            [] => q
                .rtable
                .iter()
                .find(|r| matches!(r.kind, RteKind::Subquery(_)) && !r.in_from),
            _ => None,
        };
        let then = match one.map(|r| &r.kind) {
            Some(RteKind::Subquery(s)) => self.gate(s, &visible)?,
            Some(RteKind::Cte { name }) => match visible.iter().rev().find(|c| c.name == *name) {
                Some(c) => self.gate(&c.query, &visible)?,
                None => None,
            },
            _ => None,
        };
        Ok(Some(Gate::Level {
            conds,
            then: then.map(Box::new),
        }))
    }

    /// Whether the plan starts reading a level: not under a LIMIT of none, nor under an OFFSET or
    /// a LIMIT it evaluates only as it starts (one that is no constant, or one it refuses).
    fn starts(&self, q: &Query, at: &At<'_, '_>) -> Result<bool> {
        let fits = |n: Option<&Node>, least: i64| -> Result<bool> {
            let Some(n) = n else {
                return Ok(true);
            };
            Ok(match self.node(n, at)? {
                Shape::Const(Node::Const { text: None, .. }) => true,
                Shape::Const(Node::Const { text: Some(t), .. }) => {
                    t.parse::<i64>().is_ok_and(|v| v >= least)
                }
                _ => false,
            })
        };
        Ok(fits(q.offset.as_ref(), 0)? && fits(q.limit.as_ref(), 1)?)
    }

    /// The one-time conditions evaluated in order as the plan starts: a failure of class 22 the
    /// statement's. Past one FALSE or NULL, or one the snail does not evaluate here, what it
    /// gates is left to the rows.
    fn passes(&self, g: &Gate) -> Result<()> {
        let Gate::Level { conds, then } = g;
        for c in conds {
            if self.once(c)? != Some(true) {
                return Ok(());
            }
        }
        match then {
            Some(t) => self.passes(t),
            None => Ok(()),
        }
    }

    /// A condition reading no column evaluated once over no row, as the pipeline evaluates its
    /// printed text.
    fn once(&self, n: &Node) -> Result<Option<bool>> {
        let mut p = Printer {
            cat: self.cat,
            paren: false,
            indent: false,
            level: 0,
            buf: String::new(),
            relations: Vec::new(),
            q: Default::default(),
        };
        if p.expr(n, true).is_err() {
            return Ok(None);
        }
        let Ok(e) = super::image::parse_expr(&p.buf) else {
            return Ok(None);
        };
        let Ok(types) = pipeline::typed_on(&e, &[], self.release) else {
            return Ok(None);
        };
        match pipeline::eval_typed(&e, Vec::new(), &[], self.release, &self.casings, &types) {
            Ok(pipeline::Value::Bool(b)) => Ok(Some(b)),
            Ok(_) => Ok(None),
            Err(e) if e.sqlstate().is_some_and(|s| s.starts_with("22")) => Err(e),
            Err(_) => Ok(None),
        }
    }

    /// Each argument folded, in order.
    fn each<'r>(&self, args: &[Node], at: &At<'_, 'r>) -> Result<Vec<Shape<'r>>> {
        args.iter().map(|a| self.node(a, at)).collect()
    }

    fn node<'r>(&self, n: &Node, at: &At<'_, 'r>) -> Result<Shape<'r>> {
        use Node as N;
        if !at.grouped.is_empty()
            && !matches!(n, N::Const { .. })
            && at.grouped.iter().any(|g| same(g, n))
        {
            return Ok(Shape::Varies(Vec::new()));
        }
        Ok(match n {
            N::Const { .. } => Shape::Const(n.clone()),
            N::Var { rel, attno, up, .. } => self.column(*rel, *attno, *up, at)?,
            N::Param { n: k, .. } => match at
                .args
                .and_then(|a| Some((a, a.given.get(k.checked_sub(1)?)?)))
            {
                Some((_, Given::Const(c))) => Shape::Const(c.clone()),
                Some((a, Given::Expr(e))) => self.node(e, &a.at)?,
                None => Shape::Varies(Vec::new()),
            },
            N::DomainValue { .. } | N::SqlValue { .. } => Shape::Varies(Vec::new()),
            N::CaseTest { .. } => match at.case {
                Some(c) => Shape::Const(c.clone()),
                None => Shape::Varies(Vec::new()),
            },
            N::Op { oid, args, .. } => {
                let folded = self.each(args, at)?;
                let code = self.cat.oper(*oid).map_or(0, |o| o.code);
                self.called(n, code, folded)?
            }
            N::Func {
                oid,
                args,
                variadic,
                ..
            } => {
                let folded = self.each(args, at)?;
                // A function returning RECORD is left as it is called.
                if self.cat.proc(*oid).is_some_and(|p| p.result == cat::RECORD) {
                    return Ok(varies(folded));
                }
                match crate::deparse::function_of(self.cat.catalog, *oid) {
                    Some(f) => self.own_call(n, *oid, f, args, folded, *variadic, at)?,
                    None => self.called(n, *oid, folded)?,
                }
            }
            N::Distinct { args } => {
                let folded = self.each(args, at)?;
                if folded.iter().any(|s| s.constant().is_none()) {
                    return Ok(varies(folded));
                }
                match (folded[0].is_null(), folded[1].is_null()) {
                    (true, true) => boolean(Some(false)),
                    (true, false) | (false, true) => boolean(Some(true)),
                    (false, false) => self.evaluated(n, &folded)?,
                }
            }
            N::NullIf { args, .. } => {
                let folded = self.each(args, at)?;
                if folded.iter().any(Shape::is_null) {
                    return Ok(folded[0].clone());
                }
                self.all_constant(n, folded)?
            }
            N::AnyAll { oid, args, .. } => {
                let folded = self.each(args, at)?;
                let code = self.cat.oper(*oid).map_or(0, |o| o.code);
                match self.cat.proc(code).is_some_and(|p| p.volatility == 'i') {
                    true => self.all_constant(n, folded)?,
                    false => varies(folded),
                }
            }
            N::Bool {
                op: BoolOp::And,
                args,
            } => self.and_or(&args.iter().collect::<Vec<_>>(), false, at)?,
            N::Bool {
                op: BoolOp::Or,
                args,
            } => self.and_or(&args.iter().collect::<Vec<_>>(), true, at)?,
            N::Bool {
                op: BoolOp::Not,
                args,
            } => {
                let a = self.node(&args[0], at)?;
                match a.truth() {
                    Some(t) => boolean(t.map(|t| !t)),
                    None => a,
                }
            }
            N::NullTest { arg, not } => {
                let a = self.node(arg, at)?;
                match a.constant() {
                    Some(_) if self.is_row(arg) => a,
                    Some(_) => boolean(Some(a.is_null() != *not)),
                    None => a,
                }
            }
            N::BoolTest { arg, test } => {
                let a = self.node(arg, at)?;
                match a.truth() {
                    Some(t) => boolean(Some(match test.trim() {
                        "IS TRUE" => t == Some(true),
                        "IS NOT TRUE" => t != Some(true),
                        "IS FALSE" => t == Some(false),
                        "IS NOT FALSE" => t != Some(false),
                        "IS UNKNOWN" => t.is_none(),
                        _ => t.is_some(),
                    })),
                    None => a,
                }
            }
            // A value relabelled keeps its text where both types write it alike: a reg type's name
            // is not its OID's digits.
            N::Relabel {
                arg, ty, typmod, ..
            } => match self.node(arg, at)? {
                Shape::Const(Node::Const { text, .. })
                    if text.is_none() || self.written_alike(arg.ty(), *ty) =>
                {
                    Shape::Const(Node::Const {
                        ty: *ty,
                        typmod: *typmod,
                        text,
                    })
                }
                other => varies([other]),
            },
            N::Collate { arg, .. } | N::Named { arg, .. } => self.node(arg, at)?,
            N::Io { arg, ty, .. } => {
                let a = self.node(arg, at)?;
                let output = self.cat.ty(arg.ty()).map_or(0, |t| t.output);
                let input = self.cat.ty(*ty).map_or(0, |t| t.input);
                let strict = [output, input]
                    .iter()
                    .all(|f| self.cat.proc(*f).is_some_and(|p| p.strict));
                let fixed = [output, input]
                    .iter()
                    .all(|f| self.cat.proc(*f).is_some_and(|p| p.volatility == 'i'));
                match a.constant() {
                    Some(_) if a.is_null() && strict => null_of(n),
                    Some(_) if fixed => self.evaluated(n, &[a])?,
                    _ => varies([a]),
                }
            }
            N::ArrayCoerce { arg, ty, .. } => {
                let a = self.node(arg, at)?;
                let (from, to) = (self.cat.element(arg.ty()), self.cat.element(*ty));
                match a.constant() {
                    Some(_) if self.coerces_fixed(from, to) => self.evaluated(n, &[a])?,
                    _ => varies([a]),
                }
            }
            // A domain's constraints are checked as the rows come.
            N::Domain { arg, .. } => varies([self.node(arg, at)?]),
            N::Case {
                arg,
                whens,
                default,
                ..
            } => self.case(arg.as_deref(), whens, default, at)?,
            N::Coalesce { args, .. } => {
                let mut kept = Vec::new();
                for a in args {
                    let s = self.node(a, at)?;
                    match s.constant() {
                        Some(_) if s.is_null() => {}
                        Some(_) if kept.is_empty() => return Ok(s),
                        Some(_) => return Ok(varies(kept)),
                        None => kept.push(s),
                    }
                }
                match kept.is_empty() {
                    true => null_of(n),
                    false => varies(kept),
                }
            }
            N::Array { elements: args, .. } | N::MinMax { args, .. } | N::Row { args, .. } => {
                let folded = self.each(args, at)?;
                self.all_constant(n, folded)?
            }
            N::Subscript {
                container, subs, ..
            } => {
                let mut folded = vec![self.node(container, at)?];
                for (lo, hi, _) in subs {
                    for x in lo.iter().chain(hi) {
                        folded.push(self.node(x, at)?);
                    }
                }
                self.all_constant(n, folded)?
            }
            N::Field { arg, .. } => {
                let a = self.node(arg, at)?;
                self.all_constant(n, vec![a])?
            }
            // An aggregate's arguments are read on its group's rows, before the grouping step.
            N::Agg {
                args,
                order,
                filter,
                ..
            } => {
                let inside = At {
                    grouped: &[],
                    ..*at
                };
                let mut parts = self.each(args, &inside)?;
                for s in order {
                    parts.push(self.node(&s.expr, &inside)?);
                }
                if let Some(f) = filter {
                    parts.push(self.node(f, &inside)?);
                }
                varies(parts)
            }
            N::Window { args, filter, .. } => {
                let mut parts = self.each(args, at)?;
                if let Some(f) = filter {
                    parts.push(self.node(f, at)?);
                }
                varies(parts)
            }
            // A subquery's test is folded with its expression, the subquery planned after.
            N::SubLink { test, .. } => {
                let mut parts = Vec::new();
                if let Some((left, _, _)) = test {
                    parts.push(self.node(left, at)?);
                }
                parts.push(Shape::Varies(vec![Pending {
                    link: n.clone(),
                    around: at.around.iter().copied().chain([at.scope.place]).collect(),
                    withs: at.scope.withs.clone(),
                }]));
                varies(parts)
            }
            // GROUPING names the grouping step's columns, which are not folded.
            N::Grouping { .. } => Shape::Varies(Vec::new()),
            N::List(items) => varies(self.each(items, at)?),
        })
    }

    /// A column: a pulled-up subquery's output folded where it is read, a join's column as the
    /// column it stands for; any other marked read.
    fn column<'r>(&self, rel: u32, attno: i16, up: u32, at: &At<'_, 'r>) -> Result<Shape<'r>> {
        if up > 0 {
            let place = at
                .around
                .len()
                .checked_sub(up as usize)
                .and_then(|i| at.around.get(i));
            if let Some(&place) = place {
                self.read.borrow_mut().insert((place, rel, attno));
            }
            return Ok(Shape::Varies(Vec::new()));
        }
        if let Some(sub) = at.scope.pulled.get(&rel) {
            let inner: Vec<usize> = at.around.iter().copied().chain([at.scope.place]).collect();
            let sub_at = At {
                case: None,
                grouped: &[],
                scope: sub,
                around: &inner,
                args: None,
            };
            let outputs: Vec<&Node> = sub.q.output().map(|t| &t.expr).collect();
            return match attno {
                0 => Ok(varies(
                    outputs
                        .iter()
                        .map(|o| self.node(o, &sub_at))
                        .collect::<Result<Vec<_>>>()?,
                )),
                a if a > 0 => match outputs.get(a as usize - 1) {
                    Some(o) => self.node(o, &sub_at),
                    None => Ok(Shape::Varies(Vec::new())),
                },
                _ => Ok(Shape::Varies(Vec::new())),
            };
        }
        let entry = (rel as usize)
            .checked_sub(1)
            .and_then(|i| at.scope.q.rtable.get(i));
        if let Some(RteKind::Join { vars, .. }) = entry.map(|r| &r.kind) {
            if let Some(v) = (attno as usize)
                .checked_sub(1)
                .and_then(|a| vars.get(a))
                .filter(|_| attno > 0)
            {
                return self.node(v, at);
            }
        }
        self.read.borrow_mut().insert((at.scope.place, rel, attno));
        Ok(Shape::Varies(Vec::new()))
    }

    /// A function's call, its arguments folded: NULL where it is STRICT and given a NULL
    /// constant, evaluated where it is immutable, returns no set and is given constants alone.
    fn called<'r>(&self, n: &Node, func: u32, folded: Vec<Shape<'r>>) -> Result<Shape<'r>> {
        let Some(p) = self.cat.proc(func) else {
            return Ok(varies(folded));
        };
        if p.retset {
            return Ok(varies(folded));
        }
        if p.strict && folded.iter().any(Shape::is_null) {
            return Ok(null_of(n));
        }
        if p.volatility != 'i' {
            return Ok(varies(folded));
        }
        self.all_constant(n, folded)
    }

    /// A call of one of the file's functions, its arguments folded: NULL where it is STRICT and
    /// given a NULL constant; called once where it is immutable and given constants
    /// (evaluate_function); else read through its body where 18.6 inlines it, each parameter its
    /// argument (inline_function).
    #[allow(clippy::too_many_arguments)]
    fn own_call<'r>(
        &self,
        n: &Node,
        oid: u32,
        f: &crate::catalog::Function,
        args: &[Node],
        folded: Vec<Shape<'r>>,
        variadic: bool,
        at: &At<'_, 'r>,
    ) -> Result<Shape<'r>> {
        let Some(p) = self.cat.proc(oid) else {
            return Ok(varies(folded));
        };
        if p.strict && folded.iter().any(Shape::is_null) {
            return Ok(null_of(n));
        }
        if p.volatility == 'i' && folded.iter().all(|s| s.constant().is_some()) {
            if let Shape::Const(c) = self.evaluated(n, &folded)? {
                return Ok(Shape::Const(c));
            }
        }
        if variadic
            || args.len() != f.args.len()
            || args.iter().any(|a| matches!(a, Node::Named { .. }))
            || self.inlining.borrow().contains(&oid)
        {
            return Ok(varies(folded));
        }
        let Some((true, uses)) = super::own::inlining(&f.name) else {
            return Ok(varies(folded));
        };
        // A parameter its body names more than once is inlined only for an argument that is
        // cheap, steady and holds no subquery.
        let costly = args.iter().zip(&folded).enumerate().any(|(k, (a, s))| {
            uses.get(k).copied().unwrap_or(0) > 1
                && s.constant().is_none()
                && (any_node(a, &mut |x| matches!(x, Node::SubLink { .. }))
                    || self.volatile(a)
                    || calls_in(a) > 10)
        });
        if costly {
            return Ok(varies(folded));
        }
        let Ok(body) = crate::deparse::funcs::body_query(self.cat, f) else {
            return Ok(varies(folded));
        };
        let [t] = body.targets.as_slice() else {
            return Ok(varies(folded));
        };
        let given = Args {
            given: args
                .iter()
                .zip(&folded)
                .map(|(a, s)| match s.constant() {
                    Some(c) => Given::Const(c.clone()),
                    None => Given::Expr(a),
                })
                .collect(),
            at: *at,
        };
        let inside = At {
            case: None,
            grouped: &[],
            args: Some(&given),
            ..*at
        };
        self.inlining.borrow_mut().push(oid);
        let shape = self.node(&t.expr, &inside);
        self.inlining.borrow_mut().pop();
        shape
    }

    /// AND (`or` false) or OR, flattened: folded left to right, stopping at the first constant
    /// that decides it.
    fn and_or<'r>(&self, args: &[&Node], or: bool, at: &At<'_, 'r>) -> Result<Shape<'r>> {
        let mut flat = Vec::new();
        for a in args {
            flatten(
                std::slice::from_ref(*a),
                if or { BoolOp::Or } else { BoolOp::And },
                &mut flat,
            );
        }
        let (mut null, mut kept) = (false, Vec::new());
        for a in flat {
            let s = self.node(a, at)?;
            match s.truth() {
                Some(Some(t)) if t == or => return Ok(boolean(Some(or))),
                Some(Some(_)) => {}
                Some(None) => null = true,
                None => kept.push(s),
            }
        }
        Ok(match (kept.is_empty(), null) {
            (true, false) => boolean(Some(!or)),
            (true, true) => boolean(None),
            _ => varies(kept),
        })
    }

    /// CASE: an arm whose condition folds FALSE or NULL dropped unread, the first folding TRUE
    /// its answer and the arms after it unread; a simple CASE's test folded first.
    fn case<'r>(
        &self,
        arg: Option<&Node>,
        whens: &[(Node, Node)],
        default: &Node,
        at: &At<'_, 'r>,
    ) -> Result<Shape<'r>> {
        let test = match arg {
            Some(a) => self.node(a, at)?,
            None => Shape::Varies(Vec::new()),
        };
        let inner = At {
            case: test.constant(),
            ..*at
        };
        let mut kept = vec![test.clone()];
        let mut open = false;
        for (cond, result) in whens {
            let c = self.node(cond, &inner)?;
            match c.truth() {
                Some(Some(false) | None) => continue,
                Some(Some(true)) => {
                    let r = self.node(result, &inner)?;
                    if !open {
                        return Ok(r);
                    }
                    kept.push(r);
                    return Ok(varies(kept));
                }
                None => {
                    kept.push(c);
                    kept.push(self.node(result, &inner)?);
                    open = true;
                }
            }
        }
        let d = self.node(default, &inner)?;
        if !open {
            return Ok(d);
        }
        kept.push(d);
        Ok(varies(kept))
    }

    /// Whether an array's element coercion from one type to another is immutable.
    fn coerces_fixed(&self, from: u32, to: u32) -> bool {
        if from == 0 || to == 0 {
            return false;
        }
        if from == to {
            return true;
        }
        let fixed = |f: u32| self.cat.proc(f).is_some_and(|p| p.volatility == 'i');
        match self.cat.cast(from, to) {
            Some(c) if c.method == 'f' => fixed(c.func),
            Some(c) if c.method == 'b' => true,
            _ => {
                let output = self.cat.ty(from).map_or(0, |t| t.output);
                let input = self.cat.ty(to).map_or(0, |t| t.input);
                fixed(output) && fixed(input)
            }
        }
    }

    /// Whether two types write one value as the same text: one output function, or both strings.
    fn written_alike(&self, from: u32, to: u32) -> bool {
        match (self.cat.ty(from), self.cat.ty(to)) {
            (Some(a), Some(b)) => a.output == b.output || (a.category == 'S' && b.category == 'S'),
            _ => false,
        }
    }

    /// Whether an expression is of a composite type, whose NULL test reads each field.
    fn is_row(&self, n: &Node) -> bool {
        matches!(n, Node::Row { .. })
            || self
                .cat
                .ty(n.ty())
                .is_some_and(|t| t.typtype == 'c' || t.oid == cat::RECORD)
    }

    /// Evaluated where every argument folded to a constant.
    fn all_constant<'r>(&self, n: &Node, folded: Vec<Shape<'r>>) -> Result<Shape<'r>> {
        match folded.iter().all(|s| s.constant().is_some()) {
            true => self.evaluated(n, &folded),
            false => Ok(varies(folded)),
        }
    }

    /// A node of constant arguments evaluated once over no row, each argument written as the
    /// constant it folded to: its failure the statement's, where PostgreSQL's words give it a
    /// data exception's SQLSTATE; left to the rows where the snail does not evaluate it.
    fn evaluated<'r>(&self, n: &Node, folded: &[Shape<'r>]) -> Result<Shape<'r>> {
        let mut consts = folded.iter().filter_map(Shape::constant);
        let written = with_arguments(n, &mut || consts.next().cloned());
        let mut p = Printer {
            cat: self.cat,
            paren: false,
            indent: false,
            level: 0,
            buf: String::new(),
            relations: Vec::new(),
            q: Default::default(),
        };
        if p.expr(&written, true).is_err() {
            return Ok(Shape::Varies(Vec::new()));
        }
        let Ok(e) = super::image::parse_expr(&p.buf) else {
            return Ok(Shape::Varies(Vec::new()));
        };
        let Ok(types) = pipeline::typed_on(&e, &[], self.release) else {
            return Ok(Shape::Varies(Vec::new()));
        };
        match pipeline::eval_typed(&e, Vec::new(), &[], self.release, &self.casings, &types) {
            Ok(v) => {
                let text = match (n.ty(), &v) {
                    (cat::BOOL, pipeline::Value::Bool(b)) => {
                        Some(if *b { "t" } else { "f" }.to_string())
                    }
                    _ => v.text(),
                };
                Ok(Shape::Const(Node::Const {
                    ty: n.ty(),
                    typmod: n.typmod(),
                    text,
                }))
            }
            Err(e) if e.sqlstate().is_some_and(|s| s.starts_with("22")) => Err(e),
            Err(_) => Ok(Shape::Varies(Vec::new())),
        }
    }
}

/// A WHERE's or an ON's conditions as its top-level AND lists them.
fn conjuncts(quals: Option<&Node>) -> Vec<&Node> {
    let mut out = Vec::new();
    if let Some(q) = quals {
        flatten(std::slice::from_ref(q), BoolOp::And, &mut out);
    }
    out
}

/// AND's or OR's arguments with those of the same kind inside them laid out beside them.
fn flatten<'n>(args: &'n [Node], op: BoolOp, out: &mut Vec<&'n Node>) {
    for a in args {
        match a {
            Node::Bool { op: o, args } if *o == op => flatten(args, op, out),
            _ => out.push(a),
        }
    }
}

/// The relations a FROM item reads, by their places in the range table, a join's own among them.
fn jt_rels(jt: &Jt) -> HashSet<u32> {
    match jt {
        Jt::Ref(i) => HashSet::from([*i as u32]),
        Jt::Join(j) => {
            let mut out = jt_rels(&j.left);
            out.extend(jt_rels(&j.right));
            out.insert(j.rtindex as u32);
            out
        }
    }
}

/// A set operation's arms, by their places in the range table, in order.
fn setop_leaves(t: &SetOp, out: &mut Vec<usize>) {
    match t {
        SetOp::Leaf(i) => out.push(*i),
        SetOp::Op { left, right, .. } => {
            setop_leaves(left, out);
            setop_leaves(right, out);
        }
    }
}

fn union_all(t: &SetOp) -> bool {
    match t {
        SetOp::Leaf(_) => true,
        SetOp::Op {
            op,
            all,
            left,
            right,
        } => *op == "UNION" && *all && union_all(left) && union_all(right),
    }
}

/// Whether a UNION ALL's arm may be laid out beside the query (is_safe_append_member): a FROM
/// list of one item or none, under no WHERE.
fn safe_append(s: &Query) -> bool {
    s.quals.is_none() && (s.from.is_empty() || matches!(s.from.as_slice(), [Jt::Ref(_)]))
}

/// An expression's parts, the subqueries it holds not among them.
fn children(n: &Node) -> Vec<&Node> {
    use Node as N;
    match n {
        N::Var { .. }
        | N::DomainValue { .. }
        | N::Const { .. }
        | N::CaseTest { .. }
        | N::SqlValue { .. }
        | N::Param { .. } => Vec::new(),
        N::Op { args, .. }
        | N::Func { args, .. }
        | N::Distinct { args }
        | N::NullIf { args, .. }
        | N::AnyAll { args, .. }
        | N::Bool { args, .. }
        | N::Array { elements: args, .. }
        | N::Row { args, .. }
        | N::Coalesce { args, .. }
        | N::MinMax { args, .. }
        | N::Grouping { args }
        | N::List(args) => args.iter().collect(),
        N::Relabel { arg, .. }
        | N::Io { arg, .. }
        | N::ArrayCoerce { arg, .. }
        | N::Domain { arg, .. }
        | N::NullTest { arg, .. }
        | N::BoolTest { arg, .. }
        | N::Collate { arg, .. }
        | N::Field { arg, .. }
        | N::Named { arg, .. } => vec![&**arg],
        N::Case {
            arg,
            whens,
            default,
            ..
        } => arg
            .iter()
            .map(|a| &**a)
            .chain(whens.iter().flat_map(|(c, r)| [c, r]))
            .chain([&**default])
            .collect(),
        N::Subscript {
            container, subs, ..
        } => std::iter::once(&**container)
            .chain(subs.iter().flat_map(|(lo, hi, _)| lo.iter().chain(hi)))
            .collect(),
        N::Agg {
            args,
            order,
            filter,
            ..
        } => args
            .iter()
            .chain(order.iter().map(|s| &s.expr))
            .chain(filter.iter().map(|f| &**f))
            .collect(),
        N::Window { args, filter, .. } => args.iter().chain(filter.iter().map(|f| &**f)).collect(),
        N::SubLink { test, .. } => test.iter().map(|(l, _, _)| &**l).collect(),
    }
}

/// Whether an expression or any part of it, its subqueries' insides apart, meets a test.
fn any_node(n: &Node, test: &mut dyn FnMut(&Node) -> bool) -> bool {
    test(n) || children(n).into_iter().any(|c| any_node(c, test))
}

/// How many calls an expression makes, as the price of reading it again.
fn calls_in(n: &Node) -> usize {
    let mut count = 0;
    any_node(n, &mut |x| {
        count += usize::from(matches!(
            x,
            Node::Op { .. }
                | Node::Func { .. }
                | Node::Io { .. }
                | Node::AnyAll { .. }
                | Node::Distinct { .. }
                | Node::NullIf { .. }
        ));
        false
    });
    count
}

/// The expressions a query level holds, its subqueries' insides apart.
fn roots(q: &Query) -> Vec<&Node> {
    fn ons<'q>(jt: &'q Jt, out: &mut Vec<&'q Node>) {
        if let Jt::Join(j) = jt {
            ons(&j.left, out);
            ons(&j.right, out);
            out.extend(&j.quals);
        }
    }
    let mut out: Vec<&Node> = q.targets.iter().map(|t| &t.expr).collect();
    out.extend(&q.assigned);
    for jt in &q.from {
        ons(jt, &mut out);
    }
    out.extend(
        q.quals
            .iter()
            .chain(&q.having)
            .chain(&q.offset)
            .chain(&q.limit),
    );
    for w in &q.windows {
        if let Some(f) = &w.frame {
            for b in std::iter::once(&f.start).chain(&f.end) {
                if let Bound::Preceding(n) | Bound::Following(n) = b {
                    out.push(n);
                }
            }
        }
    }
    for r in &q.rtable {
        if let RteKind::Values { rows } = &r.kind {
            out.extend(rows.iter().flatten());
        }
    }
    out
}

/// The subqueries a query level holds: its FROM list's and its expressions'.
fn nested(q: &Query) -> Vec<&Query> {
    let mut out: Vec<&Query> = q
        .rtable
        .iter()
        .filter_map(|r| match &r.kind {
            RteKind::Subquery(s) => Some(&**s),
            _ => None,
        })
        .collect();
    for n in roots(q) {
        sublinks(n, &mut out);
    }
    out
}

fn sublinks<'n>(n: &'n Node, out: &mut Vec<&'n Query>) {
    if let Node::SubLink { query, .. } = n {
        out.push(query);
    }
    for c in children(n) {
        sublinks(c, out);
    }
}

/// How often a WITH query of a name is read from a level and the levels inside it, but from its
/// own query and where another of the name hides it (cterefcount).
fn cte_reads(q: &Query, name: &str, own: Option<&Cte>) -> usize {
    let hides = |s: &Query| s.ctes.iter().any(|c| c.name == name);
    let mut reads = q
        .rtable
        .iter()
        .filter(|r| matches!(&r.kind, RteKind::Cte { name: m } if m == name))
        .count();
    reads += nested(q)
        .into_iter()
        .filter(|s| !hides(s))
        .map(|s| cte_reads(s, name, None))
        .sum::<usize>();
    for c in &q.ctes {
        if own.is_some_and(|o| std::ptr::eq(o, c)) || hides(&c.query) {
            continue;
        }
        reads += cte_reads(&c.query, name, None);
    }
    reads
}

/// The relations of the level `depth` above an expression that it reads.
fn node_rels_at(n: &Node, depth: u32, out: &mut HashSet<u32>) {
    let mut cols = HashSet::new();
    node_columns_at(n, depth, &mut cols);
    out.extend(cols.into_iter().map(|(rel, _)| rel));
}

/// The relations of the level `depth` above a query that it reads.
fn query_rels_at(q: &Query, depth: u32, out: &mut HashSet<u32>) {
    let mut cols = HashSet::new();
    query_columns_at(q, depth, &mut cols);
    out.extend(cols.into_iter().map(|(rel, _)| rel));
}

/// The relations of the level `depth` above a FROM item that its ONs read.
fn jt_quals_rels_at(jt: &Jt, depth: u32, out: &mut HashSet<u32>) {
    if let Jt::Join(j) = jt {
        jt_quals_rels_at(&j.left, depth, out);
        jt_quals_rels_at(&j.right, depth, out);
        if let Some(q) = &j.quals {
            node_rels_at(q, depth, out);
        }
    }
}

/// The columns of the level `depth` above an expression that it reads, its subqueries' among them.
fn node_columns_at(n: &Node, depth: u32, out: &mut HashSet<(u32, i16)>) {
    match n {
        Node::Var { rel, attno, up, .. } if *up == depth => {
            out.insert((*rel, *attno));
        }
        Node::SubLink { query, .. } => query_columns_at(query, depth + 1, out),
        _ => {}
    }
    for c in children(n) {
        node_columns_at(c, depth, out);
    }
}

/// The columns of the level `depth` above a query that it reads.
fn query_columns_at(q: &Query, depth: u32, out: &mut HashSet<(u32, i16)>) {
    for n in roots(q) {
        node_columns_at(n, depth, out);
    }
    for r in &q.rtable {
        if let RteKind::Subquery(s) = &r.kind {
            query_columns_at(s, depth + 1, out);
        }
    }
    for c in &q.ctes {
        query_columns_at(&c.query, depth + 1, out);
    }
}

/// A node with its arguments, in the order `node` folds them, each the next of `next`.
fn with_arguments(n: &Node, next: &mut dyn FnMut() -> Option<Node>) -> Node {
    use Node as N;
    let mut out = n.clone();
    // An argument given by name keeps its name.
    let mut put = |x: &mut Node| {
        let x = match x {
            N::Named { arg, .. } => &mut **arg,
            x => x,
        };
        if let Some(c) = next() {
            *x = c;
        }
    };
    match &mut out {
        N::Op { args, .. }
        | N::Func { args, .. }
        | N::Distinct { args }
        | N::NullIf { args, .. }
        | N::AnyAll { args, .. }
        | N::Array { elements: args, .. }
        | N::MinMax { args, .. }
        | N::Row { args, .. } => args.iter_mut().for_each(&mut put),
        N::Io { arg, .. } | N::ArrayCoerce { arg, .. } | N::Field { arg, .. } => put(arg),
        N::Subscript {
            container, subs, ..
        } => {
            put(container);
            for (lo, hi, _) in subs {
                lo.iter_mut().chain(hi.iter_mut()).for_each(&mut put);
            }
        }
        _ => {}
    }
    out
}
