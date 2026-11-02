//! A BRIN summary against a question's conditions: whether it rules an entry out, no value under the
//! entry able to hold a condition on its key.

use super::question::{Cond, Side, TextCond};
use crate::brin::{number_key, number_of, Sum, WALKED};
use crate::catalog::{Level, Shape};
use std::ops::Bound::{Excluded, Included, Unbounded};

/// Whether a value a summary sums may hold a condition on a number: for a filter, a value of the few
/// it keeps that the filter may hold.
fn may_number(sum: &Sum, c: &Cond) -> bool {
    match sum {
        Sum::Nothing => false,
        Sum::Filter { .. } => {
            let (lo, hi) = (c.kept.lo, c.kept.hi);
            match hi.checked_sub(lo) {
                Some(n) if n < 0 => false,
                Some(n) if n < WALKED => {
                    (lo..=hi).any(|v| c.holds(Some(v)) && sum.filter_has(&number_key(v)))
                }
                _ => true,
            }
        }
        Sum::Runs { runs, .. } => runs
            .iter()
            .any(|(a, b)| match (number_of(a), number_of(b)) {
                (Some(a), Some(b)) if a == b => c.holds(Some(a)),
                (Some(a), Some(b)) => a <= c.kept.hi && c.kept.lo <= b && !c.gap_holds(a, b),
                _ => true,
            }),
    }
}

/// Whether a value a summary sums may hold a condition on bytes: for a filter, the one value it keeps.
fn may_bytes(sum: &Sum, t: &TextCond) -> bool {
    match sum {
        Sum::Nothing => false,
        Sum::Filter { .. } => match (&t.lo, &t.hi) {
            (Included(a), Included(b)) if a == b => sum.filter_has(a),
            _ => true,
        },
        Sum::Runs { runs, .. } => runs.iter().any(|(a, b)| {
            if a == b {
                return t.holds(a);
            }
            let above = match &t.lo {
                Included(l) | Excluded(l) => b >= l,
                Unbounded => true,
            };
            let below = match &t.hi {
                Included(h) | Excluded(h) => a <= h,
                Unbounded => true,
            };
            above && below
        }),
    }
}

/// Whether an entry's summaries, each of the key `kept` names, rule it out for a side's conditions.
pub(super) fn rules_out(kept: &[Level], sums: &[Sum], side: &Side) -> bool {
    kept.iter().zip(sums).any(|(&l, sum)| {
        let shape = match l {
            Level::Column(c) => Shape::Column(c),
            Level::MonthUtc(c) => Shape::MonthUtc(c),
            Level::Scaled(c, k) => Shape::Scaled(c, k),
        };
        side.conds
            .iter()
            .any(|c| c.shape == shape && !may_number(sum, c))
            || (matches!(l, Level::Column(_))
                && side
                    .texts
                    .iter()
                    .any(|t| t.column == l.column() && !may_bytes(sum, t)))
    })
}
