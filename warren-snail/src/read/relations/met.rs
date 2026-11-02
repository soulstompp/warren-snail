//! Two relations met: each pair of their rows the ON holds of, or every pair; a row of the side an
//! outer join keeps that meets none, beside NULLs; then the columns the USING merges. Where the ON
//! compares a column of each side by =, a hash on one side's keys, the other's rows reading it a chunk
//! at a time. Past the room the side the hash holds comes in passes by a hash of its keys, the other
//! side's rows of the keys still to come written and read back with their pass.

use super::{conjuncts, Kind, Merged, Node, Reading};
use crate::read::pipeline::{self, parted, Key, Value};
use crate::read::spill::{Chunk, Rows, Run};
use crate::{Error, Result};
use sqlparser::ast::{BinaryOperator, Expr};
use std::collections::HashMap;

/// One side of an equality a hash meets rows by: a column at its place, or an expression.
#[derive(Clone, Copy)]
enum Keyed<'e> {
    At(usize),
    Expr(&'e Expr),
}

/// The keys a hash meets two sides' rows by: each pair the left side's and the right side's.
struct JoinKeys<'f> {
    pairs: Vec<(Keyed<'f>, Keyed<'f>)>,
    lw: usize,
    rw: usize,
    frame: &'f pipeline::Frame<'f>,
}

impl JoinKeys<'_> {
    fn key_of(&self, k: &Keyed, pair: &[Value]) -> Result<Option<Key>> {
        let v = match k {
            Keyed::At(i) => pair[*i].clone(),
            Keyed::Expr(e) => pipeline::eval(e, pair, self.frame, &pipeline::computed_none())?,
        };
        Ok((!v.is_null()).then(|| pipeline::key(&v)))
    }

    fn key(&self, pair: &[Value], right: bool) -> Result<Option<Vec<Key>>> {
        let mut k = Vec::with_capacity(self.pairs.len());
        for (a, b) in &self.pairs {
            match self.key_of(if right { b } else { a }, pair)? {
                Some(x) => k.push(x),
                None => return Ok(None),
            }
        }
        Ok(Some(k))
    }

    /// A left row's key; none where a NULL among its values meets nothing.
    fn left(&self, l: &[Value]) -> Result<Option<Vec<Key>>> {
        let mut pair = l.to_vec();
        pair.resize(self.lw + self.rw, Value::Null);
        self.key(&pair, false)
    }

    /// A right row's key.
    fn right(&self, r: &[Value]) -> Result<Option<Vec<Key>>> {
        let mut pair = vec![Value::Null; self.lw];
        pair.extend(r.iter().cloned());
        self.key(&pair, true)
    }

    fn side(&self, row: &[Value], right: bool) -> Result<Option<Vec<Key>>> {
        match right {
            true => self.right(row),
            false => self.left(row),
        }
    }
}

/// What every row a join makes passes through: whether a pair holds, and the row it makes.
struct Pairs<'p> {
    holds: &'p dyn Fn(&[Value]) -> Result<bool>,
    finish: &'p dyn Fn(Vec<Value>) -> Result<Vec<Value>>,
    lw: usize,
    width: usize,
}

impl Pairs<'_> {
    fn pair(&self, l: &[Value], r: &[Value]) -> Vec<Value> {
        let mut pair = Vec::with_capacity(self.width);
        pair.extend_from_slice(l);
        pair.extend_from_slice(r);
        pair
    }

    fn left_alone(&self, l: &[Value]) -> Result<Vec<Value>> {
        let mut pair = l.to_vec();
        pair.resize(self.width, Value::Null);
        (self.finish)(pair)
    }

    fn right_alone(&self, r: &[Value]) -> Result<Vec<Value>> {
        let mut pair = vec![Value::Null; self.lw];
        pair.extend_from_slice(r);
        (self.finish)(pair)
    }
}

/// One pass of a hash join: `build`'s rows hashed, `probe`'s read a chunk at a time, those whose keys
/// hash at `below` or past it written for a pass to come.
#[allow(clippy::too_many_arguments)]
fn probe_pass(
    keys: &JoinKeys,
    pairs: &Pairs,
    build: &[&Vec<Value>],
    build_right: bool,
    probe: &Rows,
    below: Option<u64>,
    keeps_probe: bool,
    keeps_build: bool,
    out: &mut Chunk,
) -> Result<Rows> {
    let mut map: HashMap<Vec<Key>, Vec<usize>> = HashMap::new();
    for (i, r) in build.iter().enumerate() {
        if let Some(k) = keys.side(r, build_right)? {
            map.entry(k).or_default().push(i);
        }
    }
    let mut met = vec![false; build.len()];
    let later = Run::past();
    probe.each_chunk(&mut |c| {
        for p in c {
            let k = keys.side(&p, !build_right)?;
            if let (Some(k), Some(b)) = (&k, below) {
                if parted::key_hash(k) >= b {
                    later.push(p)?;
                    continue;
                }
            }
            let mut any = false;
            if let Some(found) = k.and_then(|k| map.get(&k)) {
                for &i in found {
                    let pair = match build_right {
                        true => pairs.pair(&p, build[i]),
                        false => pairs.pair(build[i], &p),
                    };
                    if (pairs.holds)(&pair)? {
                        any = true;
                        met[i] = true;
                        out.push((pairs.finish)(pair)?)?;
                    }
                }
            }
            if !any && keeps_probe {
                out.push(match build_right {
                    true => pairs.left_alone(&p)?,
                    false => pairs.right_alone(&p)?,
                })?;
            }
        }
        Ok(())
    })?;
    if keeps_build {
        for (r, m) in build.iter().zip(&met) {
            if !m {
                out.push(match build_right {
                    true => pairs.right_alone(r)?,
                    false => pairs.left_alone(r)?,
                })?;
            }
        }
    }
    later.seal()?;
    Ok(Rows::one(later))
}

/// The two sides met by a hash: on the side the room holds whole, the left side's first; where
/// neither fits, on the right side's in passes.
fn hash_join(
    keys: &JoinKeys,
    pairs: &Pairs,
    kind: Kind,
    lrows: &Rows,
    rrows: &Rows,
    out: &mut Chunk,
) -> Result<()> {
    let keeps_left = matches!(kind, Kind::Left | Kind::Full);
    let keeps_right = matches!(kind, Kind::Right | Kind::Full);
    if rrows.held_whole() {
        return rrows.with_held(|rs| {
            probe_pass(keys, pairs, rs, true, lrows, None, keeps_left, keeps_right, out)
                .map(|_| ())
        });
    }
    if lrows.held_whole() {
        return lrows.with_held(|ls| {
            probe_pass(keys, pairs, ls, false, rrows, None, keeps_right, keeps_left, out)
                .map(|_| ())
        });
    }
    lrows.write_held()?;
    rrows.write_held()?;
    let hash_of = |r: &[Value]| -> Result<u64> {
        Ok(keys.right(r)?.map_or(0, |k| parted::key_hash(&k)))
    };
    let mut hashed = parted::Parted::new();
    rrows.each_chunk(&mut |c| {
        for r in c {
            let k = keys.right(&r)?;
            if k.is_none() && !keeps_right {
                continue;
            }
            hashed.push(k.map_or(0, |k| parted::key_hash(&k)), r)?;
        }
        Ok(())
    })?;
    let mut probe = lrows.clone();
    let mut ran = false;
    while let Some(pass) = hashed.pass(&mut |r| hash_of(r))? {
        ran = true;
        let build: Vec<&Vec<Value>> = pass.rows.iter().map(|(_, r)| r).collect();
        probe = probe_pass(
            keys, pairs, &build, true, &probe, pass.below, keeps_left, keeps_right, out,
        )?;
    }
    if !ran {
        probe_pass(keys, pairs, &[], true, &probe, None, keeps_left, keeps_right, out)?;
    }
    Ok(())
}

/// The two sides met pair by pair: each chunk of the left with each of the right, the right's rows
/// in one where the room holds them.
fn nested_loop(
    pairs: &Pairs,
    kind: Kind,
    lrows: &Rows,
    rrows: &Rows,
    out: &mut Chunk,
) -> Result<()> {
    let keeps_left = matches!(kind, Kind::Left | Kind::Full);
    let keeps_right = matches!(kind, Kind::Right | Kind::Full);
    let mut right_met = vec![false; rrows.len()];
    if rrows.held_whole() {
        rrows.with_held(|rs| {
            lrows.each_chunk(&mut |c| {
                for l in c {
                    let mut any = false;
                    for (i, r) in rs.iter().enumerate() {
                        let pair = pairs.pair(&l, r);
                        if (pairs.holds)(&pair)? {
                            any = true;
                            right_met[i] = true;
                            out.push((pairs.finish)(pair)?)?;
                        }
                    }
                    if !any && keeps_left {
                        out.push(pairs.left_alone(&l)?)?;
                    }
                }
                Ok(())
            })
        })?;
    } else {
        lrows.each_chunk(&mut |c| {
            let mut left_met = vec![false; c.len()];
            let mut at = 0;
            rrows.each_chunk(&mut |rc| {
                for (i, r) in rc.iter().enumerate() {
                    for (k, l) in c.iter().enumerate() {
                        let pair = pairs.pair(l, r);
                        if (pairs.holds)(&pair)? {
                            left_met[k] = true;
                            right_met[at + i] = true;
                            out.push((pairs.finish)(pair)?)?;
                        }
                    }
                }
                at += rc.len();
                Ok(())
            })?;
            if keeps_left {
                for (l, m) in c.iter().zip(&left_met) {
                    if !m {
                        out.push(pairs.left_alone(l)?)?;
                    }
                }
            }
            Ok(())
        })?;
    }
    if keeps_right {
        let mut at = 0;
        rrows.each_chunk(&mut |c| {
            for (r, m) in c.iter().zip(&right_met[at..]) {
                if !m {
                    out.push(pairs.right_alone(r)?)?;
                }
            }
            at += c.len();
            Ok(())
        })?;
    }
    Ok(())
}

impl Reading<'_> {
    /// Two relations met, the rows they make handed to `each` a chunk at a time. The right side's
    /// rows are made for each row of the left where it is lateral.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn met(
        &mut self,
        node: &Node,
        left: &Node,
        right: &Node,
        kind: Kind,
        on: Option<&Expr>,
        merged: &[Merged],
        lrows: Rows,
        rrows: Option<Rows>,
        before: Option<(usize, &[Value])>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<()> {
        let frame_columns = self.from.frame_columns(node.at, left.width + right.width);
        let frame = pipeline::Frame {
            columns: frame_columns,
            params: self.params,
            release: self.release,
            casings: self.casings,
            types: self.types,
            now: self.now,
            subqueries: None,
        };
        let none = pipeline::computed_none();
        // The pairs of places a USING meets, compared by =.
        let usings: Vec<(usize, usize)> = merged
            .iter()
            .map(|m| (m.left - node.at, m.right - node.at))
            .collect();
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
        let pairs = Pairs {
            holds: &holds,
            finish: &finish,
            lw: left.width,
            width,
        };
        let mut out = Chunk::new(each);
        match rrows {
            Some(rrows) => match self.join_keys(left, on, &usings, &frame) {
                Some(keys) => hash_join(&keys, &pairs, kind, &lrows, &rrows, &mut out)?,
                None => nested_loop(&pairs, kind, &lrows, &rrows, &mut out)?,
            },
            None => {
                if matches!(kind, Kind::Right | Kind::Full) {
                    return Err(Error::new(
                        "invalid reference to FROM-clause entry: a RIGHT or FULL JOIN to a LATERAL \
                         function",
                    ));
                }
                lrows.each_chunk(&mut |c| {
                    for l in c {
                        // The rows before it, and the left side's.
                        let mut ctx: Vec<Value> =
                            before.map(|(_, r)| r.to_vec()).unwrap_or_default();
                        let start = before.map_or(left.at, |(s, _)| s);
                        ctx.extend(l.iter().cloned());
                        let rs = self.place(right, Some((start, &ctx)), false)?;
                        let mut any = false;
                        rs.each_chunk(&mut |rc| {
                            for r in rc {
                                let pair = pairs.pair(&l, &r);
                                if holds(&pair)? {
                                    any = true;
                                    out.push(finish(pair)?)?;
                                }
                            }
                            Ok(())
                        })?;
                        if !any && kind == Kind::Left {
                            out.push(pairs.left_alone(&l)?)?;
                        }
                    }
                    Ok(())
                })?;
            }
        }
        out.finish()
    }

    /// The key each conjunct of the ON comparing a column of each side by = gives, with the USING's;
    /// none where no conjunct is one, or one of them compares under a library's collation or across
    /// types whose keys differ.
    fn join_keys<'f>(
        &self,
        left: &Node,
        on: Option<&'f Expr>,
        usings: &[(usize, usize)],
        frame: &'f pipeline::Frame<'f>,
    ) -> Option<JoinKeys<'f>> {
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
            return None;
        }
        Some(JoinKeys {
            pairs,
            lw,
            rw: frame.columns.len() - lw,
            frame,
        })
    }
}
