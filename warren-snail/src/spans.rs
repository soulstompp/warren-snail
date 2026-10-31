//! The span a GiST or SP-GiST strand keeps of each column it names, block by block: a range's and a
//! multirange's least lower bound and greatest upper bound, an inet's first and last address; and
//! whether a block of that span may hold a row an operator's condition keeps.

use crate::catalog::{ColumnType, Method, Order, Table};
use crate::composed::Composed;
use crate::directory::Span;
use crate::read::statement::Datum;
use crate::textual::Textual;
use crate::values::Values;

/// Whether a strand of `method` keeps a span of a column of type `ty`.
fn spanned(method: Method, ty: ColumnType) -> bool {
    matches!(method, Method::Gist | Method::Spgist) && (is_inet(ty) || is_period(ty))
}

/// Whether a column may be a key's period: a range or a multirange.
pub(crate) fn is_period(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Composed(r)
            if matches!(r.def(), Composed::Range { .. } | Composed::Multirange { .. })
    )
}

pub(crate) fn is_inet(ty: ColumnType) -> bool {
    matches!(
        ty,
        ColumnType::Textual(Textual::Inet) | ColumnType::Textual(Textual::Cidr)
    )
}

/// The columns whose span an order keeps: its levels of a range, a multirange or an inet, where
/// its index is a GiST or an SP-GiST.
pub(crate) fn columns(table: &Table, order: &Order) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for l in &order.levels {
        let c = l.column();
        if spanned(order.method, table.columns[c].ty) && !out.contains(&c) {
            out.push(c);
        }
    }
    out
}

/// The slots of the first leaves of the columns whose span an order keeps.
pub(crate) fn slots(table: &Table, order: &Order) -> Vec<u32> {
    columns(table, order)
        .into_iter()
        .filter_map(|c| order.leaf_slot(table, c))
        .map(|s| s as u32)
        .collect()
}

/// A value's bounds: none for NULL; an empty value's, none within.
pub(crate) fn bounds(ty: ColumnType, d: &Datum) -> Option<Option<(Vec<u8>, Vec<u8>)>> {
    match d {
        Datum::Null => None,
        Datum::Text(t) if is_inet(ty) => Some(Some(crate::inet::inet_span(t))),
        Datum::Compound(c) => Some(crate::composed::span_bounds(ty, &c.item)),
        _ => Some(Some((Vec::new(), vec![u8::MAX]))),
    }
}

/// A span widened to hold a value.
pub(crate) fn widen(span: &mut Span, ty: ColumnType, d: &Datum) {
    match bounds(ty, d) {
        None => {}
        Some(None) => span.widen(None),
        Some(Some((lo, hi))) => span.widen(Some((&lo, &hi))),
    }
}

/// The span of a column's values at the rows given.
pub(crate) fn of_rows(ty: ColumnType, v: &Values, rows: &[usize]) -> Span {
    let mut span = Span::default();
    for &r in rows {
        widen(&mut span, ty, &crate::values::datum_of(v, r, ty));
    }
    span
}

/// The value a condition compares a spanned column with, as a span: none where it is empty.
#[derive(Clone, Debug)]
pub(crate) enum Probe {
    Empty,
    Span(Vec<u8>, Vec<u8>),
}

/// The bytes of a bound at its value, whichever side it bounds.
fn value_of(b: &[u8]) -> &[u8] {
    &b[..b.len().saturating_sub(1)]
}

/// Whether a block whose column spans `span` may hold a row that `column op probe` keeps, as
/// PostgreSQL's GiST asks an inner entry: a range's and a multirange's `&&`, `@>`, `<@`, `=`,
/// `<<`, `>>`, `&<`, `&>` and `-|-`; an inet's `&&`, `<<`, `<<=`, `>>` and `>>=`, each of which
/// asks that the networks share an address.
pub(crate) fn may_hold(op: &str, span: &Span, probe: &Probe, inet: bool) -> bool {
    let hull = span
        .present
        .then_some((span.lo.as_slice(), span.hi.as_slice()));
    let overlap = |q: &Probe| match (hull, q) {
        (Some((l, u)), Probe::Span(ql, qu)) => l <= qu.as_slice() && ql.as_slice() <= u,
        _ => false,
    };
    if inet {
        return overlap(probe);
    }
    match (op, probe) {
        ("&&", q) => overlap(q),
        ("@>", Probe::Empty) => true,
        ("@>" | "=", Probe::Span(ql, qu)) => {
            hull.is_some_and(|(l, u)| l <= ql.as_slice() && qu.as_slice() <= u)
        }
        ("=", Probe::Empty) => span.empty,
        ("<@", q) => span.empty || overlap(q),
        (_, Probe::Empty) => false,
        (op, Probe::Span(ql, qu)) => {
            let Some((l, u)) = hull else {
                return false;
            };
            let (ql, qu) = (ql.as_slice(), qu.as_slice());
            match op {
                "<<" => l < ql,
                "&<" => l <= qu,
                "&>" => u >= ql,
                ">>" => u > qu,
                "-|-" => {
                    overlap(probe) || value_of(u) == value_of(ql) || value_of(qu) == value_of(l)
                }
                _ => true,
            }
        }
    }
}
