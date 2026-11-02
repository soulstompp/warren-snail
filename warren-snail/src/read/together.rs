//! Snails opened each on its own and read together, as a host that holds one handle a file reads
//! them: their roots taken at one moment, a question on their tables answered on the roots they
//! hold, and priced from their directories alone, no column page read.

use super::across::{carried_root, Snails};
use super::join::{ask_join_within, core_of, needed_of, read_order, reading};
use super::question;
use super::walk::Want;
use super::{Round, Snail};
use crate::catalog::Table;
use crate::file::Root;
use crate::Result;
use arrow::array::RecordBatch;

/// The file a snail was opened on, as a refusal names it.
fn shown(snails: &[&Snail], i: usize) -> String {
    snails[i].reader.path().display().to_string()
}

/// Several snails, each opened on its own, their roots taken at one moment: every root was read as
/// its snail opened; each is read again, and where one changed, every one afresh and again, until
/// none changed between. A root a decision among them carries for one of them, and that snail has
/// not taken, is then read as written, where one of them alone holds the table the decision names
/// it by. The root pages each read past its open.
pub fn at_one_moment(snails: &mut [Snail]) -> Result<Vec<u64>> {
    let before: Vec<u64> = snails.iter().map(Snail::pages_read).collect();
    if snails.len() > 1 {
        loop {
            let mut again = false;
            for s in snails.iter() {
                again |= s.reader.peek_root()?.generation != s.root.generation;
            }
            if !again {
                break;
            }
            for s in snails.iter_mut() {
                s.renew()?;
            }
        }
        let roots: Vec<Root> = snails.iter().map(|s| s.root.clone()).collect();
        let owed = {
            let refs: Vec<&Snail> = snails.iter().collect();
            let holds = |name: &str| {
                let mut at = refs
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| s.catalog.tables.iter().any(|t| t.name == name))
                    .map(|(i, _)| i);
                match (at.next(), at.next()) {
                    (Some(i), None) => Some(i),
                    _ => None,
                }
            };
            let mut owed = Vec::with_capacity(refs.len());
            for i in 0..refs.len() {
                let mut found = None;
                for (d, root) in roots.iter().enumerate().filter(|(d, _)| *d != i) {
                    found = carried_root(
                        &refs,
                        &holds,
                        &|k| shown(&refs, k),
                        (d, root),
                        i,
                        roots[i].generation,
                    )?;
                    if found.is_some() {
                        break;
                    }
                }
                owed.push(found);
            }
            owed
        };
        for (s, root) in snails.iter_mut().zip(owed) {
            if let Some(root) = root {
                s.adopt(root)?;
            }
        }
    }
    Ok(snails
        .iter()
        .zip(before)
        .map(|(s, b)| s.pages_read() - b)
        .collect())
}

/// A question on the tables of snails read together, each snail read on the root it holds, none
/// read again: one round. A question on one snail's tables is read as that snail reads it; one on
/// several snails' tables as a join, each table from its own snail, each snail's part in the
/// round's `snails`. Refused where the snails come from two releases, or two of them hold one
/// name.
pub fn answer_together(snails: &[&Snail], sql: &str) -> Result<(RecordBatch, Round)> {
    if let [one] = snails {
        return one.answer(sql, None);
    }
    let (catalog, at) = Snails::merged(snails, &|i| shown(snails, i))?;
    let q = question::parse(&catalog, sql)?;
    let at: Vec<(usize, usize)> = q.sides.iter().map(|s| at[s.table]).collect();
    let mut read: Vec<usize> = at.iter().map(|&(s, _)| s).collect();
    read.sort();
    read.dedup();
    match read.as_slice() {
        [one] => snails[*one].answer(sql, None),
        _ => ask_join_within(snails, &at, &q, false, None),
    }
}

/// One table's part of a round priced from its directory alone: the order it would be read along
/// by its own conditions, the directory pages the pricing read, and its column pages. A table read
/// by its own conditions is priced exactly. A table kept to the keys of a table read before it is
/// priced between the fewest pages those keys allow, none, and the most, those its own conditions
/// read; its estimate is the round of one key along the order those keys fix, once for each row
/// the counts estimate the table it is kept to holds, within the two.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct TablePrice {
    pub table: String,
    /// Its place among the question's tables, as FROM names them.
    pub side: usize,
    /// Where it is kept, the place of the table whose keys it is kept to.
    pub kept_by: Option<usize>,
    pub order: String,
    /// Whether it is kept to the keys of a table read before it, its walk narrowed by them.
    pub kept: bool,
    pub directory_pages: u64,
    pub column_pages_low: u64,
    pub column_pages_high: u64,
    pub column_pages_estimate: u64,
    /// The column pages of the round of one key, where it is kept.
    pub key_column_pages: Option<u64>,
    /// The rows its own conditions hold, as the counts bound and estimate them, and the table's.
    pub rows_low: u64,
    pub rows_high: u64,
    pub rows_estimate: u64,
    pub table_rows: u64,
}

/// A question on the tables of snails read together, priced from their directories alone, on the
/// roots they hold: each table as the round would read it, in the order it would read them.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Priced {
    pub tables: Vec<TablePrice>,
}

impl Priced {
    /// The sums over its tables: directory pages, and the fewest, the most and the estimated
    /// column pages.
    pub fn directory_pages(&self) -> u64 {
        self.tables.iter().map(|t| t.directory_pages).sum()
    }

    pub fn column_pages(&self) -> (u64, u64, u64) {
        self.tables.iter().fold((0, 0, 0), |(l, h, e), t| {
            (
                l + t.column_pages_low,
                h + t.column_pages_high,
                e + t.column_pages_estimate,
            )
        })
    }

    /// The order each table would be read along, the first read first.
    pub fn order(&self) -> String {
        self.tables
            .iter()
            .map(|t| t.order.as_str())
            .collect::<Vec<_>>()
            .join("+")
    }
}

/// A question on the tables of snails read together priced from their directories alone, on the
/// roots they hold, no column page read: the order the round reads its tables in, and each table's
/// part. Refused as `answer_together` refuses it.
pub fn price_together(snails: &[&Snail], sql: &str) -> Result<Priced> {
    let (catalog, at) = Snails::merged(snails, &|i| shown(snails, i))?;
    let q = question::parse(&catalog, sql)?;
    let at: Vec<(usize, usize)> = q.sides.iter().map(|s| at[s.table]).collect();
    if let [(s, _)] = at.as_slice() {
        let p = snails[*s].price(sql, None)?;
        return Ok(Priced {
            tables: vec![TablePrice {
                table: catalog.tables[q.sides[0].table].name.clone(),
                side: 0,
                kept_by: None,
                order: p.order,
                kept: false,
                directory_pages: p.directory_pages,
                column_pages_low: p.column_pages,
                column_pages_high: p.column_pages,
                column_pages_estimate: p.column_pages,
                key_column_pages: None,
                rows_low: p.rows_low,
                rows_high: p.rows_high,
                rows_estimate: p.rows_estimate,
                table_rows: p.table_rows,
            }],
        });
    }
    let tables: Vec<&Table> = at
        .iter()
        .map(|&(s, t)| &snails[s].catalog.tables[t])
        .collect();
    let sides: Vec<question::Side> = q
        .sides
        .iter()
        .zip(&at)
        .map(|(side, &(_, t))| {
            let mut side = side.clone();
            side.table = t;
            side
        })
        .collect();
    let order = read_order(&q, &tables);
    let (_, stream) = core_of(&q, &order);
    let last = *order.last().unwrap();
    let (needed, _) = needed_of(&q);
    let mut rows = vec![0u64; q.sides.len()];
    let mut out = Vec::with_capacity(order.len());
    for (i, &side) in order.iter().enumerate() {
        let snail = snails[at[side].0];
        let table = tables[side];
        let r = reading(&q, &order, i, table, stream && side == last, &needed[side]);
        let want = Want {
            table,
            side: &sides[side],
            columns: r.columns.iter().map(|c| c.0).collect(),
            countable: None,
            keys: None,
        };
        let before = snail.touched();
        let part = snail.part(&want, None)?;
        let own = part.plan.column_pages;
        rows[side] = (part.plan.rows_estimate.round() as u64).clamp(part.rows.0, part.rows.1);
        let point = match (r.by.first(), r.led.as_ref()) {
            (Some(&(by, _)), Some(meets)) => {
                let given: Vec<usize> = meets.iter().map(|m| m.a.column).collect();
                snail
                    .point_of(&want, &given)?
                    .map(|p| (by, p.column_pages, p.order))
            }
            _ => None,
        };
        let (low, estimate) = match &point {
            Some((by, one, _)) => (0, rows[*by].saturating_mul(*one).min(own)),
            None => (own, own),
        };
        out.push(TablePrice {
            table: table.name.clone(),
            side,
            kept_by: point.as_ref().map(|p| p.0),
            order: match &point {
                Some((_, _, along)) => along.clone(),
                None => table.orders[part.plan.walk.order].name.clone(),
            },
            kept: point.is_some(),
            directory_pages: snail.touched() - before,
            column_pages_low: low,
            column_pages_high: own,
            column_pages_estimate: estimate,
            key_column_pages: point.as_ref().map(|p| p.1),
            rows_low: part.rows.0,
            rows_high: part.rows.1,
            rows_estimate: rows[side],
            table_rows: part.table_rows,
        });
    }
    Ok(Priced { tables: out })
}
