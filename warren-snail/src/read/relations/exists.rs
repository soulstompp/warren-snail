//! An EXISTS or NOT EXISTS whose subquery is tied to the query around it by =s, read as the join it
//! is: the rows its FROM gives that its own conjuncts keep, read once a round, each by the values
//! its ties' own sides give; a row asking meets those of the values its ties' other sides give, and
//! a conjunct naming both is met on each pair.

use super::{read_from, tied_keeps, Accrued, Asked, From, Ons, Outer, Ready};
use crate::read::pipeline::{self, Value};
use crate::Result;
use sqlparser::ast::{BinaryOperator, Expr, SetExpr};
use std::ops::ControlFlow;

/// How an EXISTS's subquery meets the query around it: each = tying an expression of its own
/// columns to one of that query's alone, with the place among the values asking of the column that
/// side is, where it is one; its conjuncts of its own columns; and every other, met on each pair.
pub(in crate::read) struct Joined<'s> {
    ties: Vec<(&'s Expr, &'s Expr, Option<usize>)>,
    own: Vec<&'s Expr>,
    pairs: Vec<&'s Expr>,
}

impl Joined<'_> {
    /// Whether a pair is met by more than the values of its ties.
    pub(in crate::read) fn by_pairs(&self) -> bool {
        !self.pairs.is_empty()
    }

    /// How many ties it has.
    pub(in crate::read) fn ties(&self) -> usize {
        self.ties.len()
    }
}

/// What an expression reads of a subquery's columns: of its FROM's, of the query around it's, of
/// both or of none; none where a name finds no one column.
#[derive(Clone, Copy, PartialEq)]
enum Reads {
    Nothing,
    Own,
    Around,
    Both,
}

fn reads_of(e: &Expr, from: &From) -> Option<Reads> {
    let mut read = Vec::new();
    pipeline::columns_read(e, &mut read);
    let (mut own, mut around) = (false, false);
    for (q, n) in &read {
        match pipeline::naming(
            from.columns
                .iter()
                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
            q.as_deref(),
            n,
        )
        .as_slice()
        {
            [i] if *i < from.outer_at => own = true,
            [_] => around = true,
            _ => return None,
        }
    }
    Some(match (own, around) {
        (false, false) => Reads::Nothing,
        (true, false) => Reads::Own,
        (false, true) => Reads::Around,
        (true, true) => Reads::Both,
    })
}

impl<'s> Ready<'s> {
    /// How it meets the query around it as an EXISTS's subquery read as a join: one plain SELECT
    /// whose FROM reads nothing of that query, returning each row its WHERE keeps, calling nothing
    /// volatile, its WHERE's conjuncts holding no subquery and one of them at least an = tying it
    /// to that query. None where it is not.
    pub(in crate::read) fn joined(&self, release: u8) -> Option<Joined<'s>> {
        if !self.keepable || !self.p.returns_each_kept() || self.step.query.with.is_some() {
            return None;
        }
        let volatile = sqlparser::ast::visit_expressions(&self.step.query, |e| match e {
            Expr::Function(f)
                if crate::read::own::may_be_volatile(&pipeline::function_name(f), release)
                    || pipeline::draws(e) =>
            {
                ControlFlow::Break(())
            }
            _ => ControlFlow::Continue(()),
        })
        .is_break();
        if volatile {
            return None;
        }
        let SetExpr::Select(s) = self.step.query.body.as_ref() else {
            return None;
        };
        let mut conds = Vec::new();
        if let Some(w) = &s.selection {
            super::conjuncts(w, &mut conds);
        }
        let from = self.from;
        let types = &self.p.types;
        let mut joined = Joined {
            ties: Vec::new(),
            own: Vec::new(),
            pairs: Vec::new(),
        };
        for c in conds {
            let mut held = Vec::new();
            pipeline::subqueries_in(c, &mut held);
            if !held.is_empty() {
                return None;
            }
            if let Expr::BinaryOp {
                left,
                op: BinaryOperator::Eq,
                right,
            } = c
            {
                let keyed = match (
                    types.get(&(&**left as *const Expr)),
                    types.get(&(&**right as *const Expr)),
                ) {
                    (Some(x), Some(y)) => x == y || (x.is_integer() && y.is_integer()),
                    _ => false,
                } && !self.casings.compared_under_library(c);
                let sides = (reads_of(left, from)?, reads_of(right, from)?);
                let tie = match sides {
                    (Reads::Own, Reads::Around) => Some((&**left, &**right)),
                    (Reads::Around, Reads::Own) => Some((&**right, &**left)),
                    _ => None,
                };
                if let (Some((a, b)), true) = (tie, keyed) {
                    let at = pipeline::column_of(b).and_then(|(q, n)| {
                        match pipeline::naming(
                            from.columns
                                .iter()
                                .map(|c| (c.qualifier.as_deref(), c.name.as_str())),
                            q.as_deref(),
                            &n,
                        )
                        .as_slice()
                        {
                            [i] if *i >= from.outer_at => Some(i - from.outer_at),
                            _ => None,
                        }
                    });
                    joined.ties.push((a, b, at));
                    continue;
                }
            }
            match reads_of(c, from)? {
                Reads::Nothing | Reads::Own => joined.own.push(c),
                Reads::Around | Reads::Both => joined.pairs.push(c),
            }
        }
        (!joined.ties.is_empty()).then_some(joined)
    }

    fn frame<'f>(&'f self, asked: Asked<'f>) -> pipeline::Frame<'f> {
        pipeline::Frame {
            columns: self.from.frame_columns(0, self.from.columns.len()),
            params: asked.params,
            release: asked.release,
            casings: &self.casings,
            types: &self.p.types,
            now: asked.now,
            subqueries: None,
            reads: Some(asked.reads),
            below: None,
        }
    }

    /// Hands `each` the values its ties' own sides give each row its FROM gives that its own
    /// conjuncts keep, none NULL, and the row where pairs are met by more: its FROM read once, kept
    /// to the keys `outer.kept` foresaw, `rows_most` told the most it may give.
    pub(in crate::read) fn joined_rows(
        &self,
        j: &Joined,
        asked: Asked,
        outer: &Outer,
        accrued: &Accrued,
        rows_most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Value>, Vec<Value>) -> Result<()>,
    ) -> Result<()> {
        if self.shut {
            return Ok(());
        }
        let from = self.from;
        let (casings, types) = (&self.casings, &self.p.types);
        let keeps = tied_keeps(&self.tied, from, outer, asked, casings, types)?;
        let on_subs: Vec<(*const super::Node, crate::read::subqueries::Subs)> = self
            .on_plans
            .iter()
            .map(|(n, columns, p)| {
                (
                    *n,
                    crate::read::subqueries::Subs::new(
                        p, asked, self.made, casings, columns, accrued,
                    ),
                )
            })
            .collect();
        let ons: Ons = on_subs
            .iter()
            .map(|(n, s)| (*n, s as &dyn pipeline::Subqueries))
            .collect();
        let frame = self.frame(asked);
        let none = pipeline::computed_none();
        let width = from.columns.len();
        let by_pairs = j.by_pairs();
        read_from(
            asked,
            from,
            &self.named,
            self.wheres.clone(),
            casings,
            types,
            self.made,
            accrued,
            &self.step.laterals,
            Some(outer),
            &ons,
            keeps,
            rows_most,
            None,
            None,
            None,
            &mut |c, _| {
                'rows: for r in c {
                    let mut row = r;
                    let own = row.len();
                    row.resize(width, Value::Null);
                    for w in &j.own {
                        if pipeline::holds(w, &row, &frame, &none)? != Some(true) {
                            continue 'rows;
                        }
                    }
                    let mut tie = Vec::with_capacity(j.ties.len());
                    for (a, _, _) in &j.ties {
                        let v = pipeline::eval(a, &row, &frame, &none)?;
                        if v.is_null() {
                            continue 'rows;
                        }
                        tie.push(v);
                    }
                    row.truncate(own);
                    each(tie, if by_pairs { row } else { Vec::new() })?;
                }
                Ok(())
            },
        )
    }

    /// The values its ties' other sides give a set of values asking; none where one is NULL, which
    /// meets no row.
    pub(in crate::read) fn joined_values(
        &self,
        j: &Joined,
        asked: Asked,
        values: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        let mut out = Vec::with_capacity(j.ties.len());
        let mut row: Option<Vec<Value>> = None;
        for (_, b, at) in &j.ties {
            let v = match at {
                Some(i) => values[*i].clone(),
                None => {
                    let row = row.get_or_insert_with(|| {
                        let mut r = vec![Value::Null; self.from.outer_at];
                        r.extend(values.iter().cloned());
                        r
                    });
                    pipeline::eval(b, row, &self.frame(asked), &pipeline::computed_none())?
                }
            };
            if v.is_null() {
                return Ok(None);
            }
            out.push(v);
        }
        Ok(Some(out))
    }

    /// Whether a row its FROM gives and a set of values asking meet by every conjunct naming the
    /// query around it.
    pub(in crate::read) fn pair_holds(
        &self,
        j: &Joined,
        asked: Asked,
        row: &[Value],
        values: &[Value],
    ) -> Result<bool> {
        let mut pair = row.to_vec();
        pair.extend(values.iter().cloned());
        let frame = self.frame(asked);
        let none = pipeline::computed_none();
        for w in &j.pairs {
            if pipeline::holds(w, &pair, &frame, &none)? != Some(true) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
