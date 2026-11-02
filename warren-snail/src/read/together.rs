//! Snails opened each on its own and read together, as a host that holds one handle a file reads
//! them: their roots taken at one moment, a question on their tables answered on the roots they
//! hold, and priced from their directories alone, no column page read.

use super::across::{carried_root, Snails};
use super::join::{self, ask_join_within, core_of, needed_of, read_order, reading};
use super::pipeline::{self, batch_of, relation_of, Relation};
use super::spill::{rows_width, Handed, Rows, Run};
use super::walk::Want;
use super::{computed, question, relations, row_bytes, spelled, subqueries, Round, Snail};
use crate::catalog::Table;
use crate::file::Root;
use crate::room::{Room, Sizes};
use crate::{Error, Result};
use arrow::array::RecordBatch;
use std::collections::VecDeque;
use std::rc::Rc;

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

/// What a round makes of its answer, as it makes it: the batches its room holds, and the rows past it
/// written, or those a pipeline left lying in the room and past it.
struct Lying {
    room: Rc<Room>,
    batches: VecDeque<(RecordBatch, u64)>,
    past: Option<Run>,
    rows: Option<Rows>,
    names: Vec<String>,
}

impl Lying {
    /// A batch of the answer: held where the room has room for it and no row before it was written;
    /// its rows written otherwise.
    fn take(&mut self, b: RecordBatch) -> Result<()> {
        if b.num_rows() == 0 {
            return Ok(());
        }
        if self.names.is_empty() {
            self.names = b.schema().fields().iter().map(|f| f.name().clone()).collect();
        }
        let bytes = b.get_array_memory_size() as u64;
        if self.past.is_none() && self.room.hold(bytes) {
            self.batches.push_back((b, bytes));
            return Ok(());
        }
        let run = self.past.get_or_insert_with(Run::past);
        for row in relation_of(&b)?.rows {
            run.push(row)?;
        }
        Ok(())
    }

    /// Those rows written, ready to read back after the batches held.
    fn done(mut self) -> Result<Answers> {
        let mut rows = self.rows.take();
        if let Some(run) = self.past.take() {
            run.seal()?;
            rows = Some(Rows::one(run));
        }
        Ok(Answers {
            batches: self.batches,
            rows: rows.map(Rows::handed),
            names: self.names,
        })
    }
}

/// A round's answer once it is made: the batches its room holds, then its rows in the room and past
/// it, a chunk at a time.
struct Answers {
    batches: VecDeque<(RecordBatch, u64)>,
    rows: Option<Handed>,
    names: Vec<String>,
}

/// A round's answer handed out a batch at a time, its reads done: `next_batch` hands out each, read
/// back from what the round's room holds and from what it wrote past it, and `round` says what it has
/// read and written so far. The round's spill closes as this is dropped.
#[non_exhaustive]
pub struct Answering {
    room: Rc<Room>,
    round: Round,
    answers: Answers,
    in_hand: u64,
}

impl Answering {
    /// The next batch of the answer; none once every row has come.
    pub fn next_batch(&mut self) -> Result<Option<RecordBatch>> {
        self.room.landed(std::mem::take(&mut self.in_hand));
        if let Some((b, bytes)) = self.answers.batches.pop_front() {
            self.room.let_go(bytes);
            self.room.handed(bytes);
            self.in_hand = bytes;
            return Ok(Some(b));
        }
        let Some(rows) = self.answers.rows.as_mut() else {
            return Ok(None);
        };
        let Some(rows) = rows.next_chunk()? else {
            return Ok(None);
        };
        self.in_hand = rows_width(&rows);
        self.room.handed(self.in_hand);
        batch_of(&Relation {
            names: self.answers.names.clone(),
            rows,
            order: None,
        })
        .map(Some)
    }

    /// What the round has read, the most it has held, and the pages it has written past its room and
    /// read back.
    pub fn round(&self) -> Round {
        let mut round = self.round.clone();
        round.bytes_held = self.room.most();
        (round.spill_pages_written, round.spill_pages_read) = self.room.spill_pages();
        round
    }
}

impl Drop for Answering {
    fn drop(&mut self) {
        self.room.close_spill();
    }
}

/// A question on the tables of snails read together, on the roots they hold: one round in a room of
/// its own that `sizes` gives, read whole before this returns, what it holds of its answer counted in
/// that room and the rest written past it. A second round may begin while its answer is handed out.
pub fn begin_together(snails: &[&Snail], sql: &str, sizes: Sizes) -> Result<Answering> {
    let room = Room::new(sizes);
    room.hold_answer();
    let _rooms = InRoom::of(snails, &room);
    let entered = crate::room::entered(&room);
    let mut lying = Lying {
        room: room.clone(),
        batches: VecDeque::new(),
        past: None,
        rows: None,
        names: Vec::new(),
    };
    let round = answer_lying(snails, sql, &mut lying)?;
    let answers = lying.done()?;
    room.let_go_pages();
    drop(entered);
    Ok(Answering {
        room,
        round,
        answers,
        in_hand: 0,
    })
}

/// While it lives, snails read in a room given: each its own again as it goes.
struct InRoom<'s>(Vec<(&'s Snail, Rc<Room>)>);

impl<'s> InRoom<'s> {
    fn of(snails: &[&'s Snail], room: &Rc<Room>) -> InRoom<'s> {
        InRoom(
            snails
                .iter()
                .map(|&s| {
                    let own = s.reader.room();
                    s.reader.hold_rounds_in(room.clone());
                    (s, own)
                })
                .collect(),
        )
    }
}

impl Drop for InRoom<'_> {
    fn drop(&mut self) {
        for (s, own) in self.0.drain(..) {
            s.reader.hold_rounds_in(own);
        }
    }
}

/// A question read as the snail reads it, its answer laid in `lying`. Where it reads as a round on its
/// tables' orders it is read so, a join's rows laid out a block's at a time, and a grouping's stopping
/// should its groups pass the room; what it does not read so, and what stops, a pipeline over the
/// snails' relations reads.
fn answer_lying(snails: &[&Snail], sql: &str, lying: &mut Lying) -> Result<Round> {
    question::unlocked_words(sql)?;
    relations::unread_clauses(sql)?;
    let sql = &*spelled::spelled(sql)?;
    let parsed =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, sql)?;
    let [sqlparser::ast::Statement::Query(asked)] = parsed.as_slice() else {
        return Err(Error::new("a question is one SELECT"));
    };
    question::unlocked(asked)?;
    let (catalog, tables_at) = Snails::merged(snails, &|i| shown(snails, i))?;
    let by_pipeline = computed(&catalog, asked)?.is_some()
        || subqueries::held(asked)
        || pipeline::is_pipeline(asked)
        || relations::is_set_operation(asked)
        || pipeline::tests_composite_null(&catalog, asked)
        || pipeline::reads_per_row(&catalog, asked)
        || relations::names_a_view(&catalog, asked);
    if let (false, Ok(q)) = (by_pipeline, question::parse(&catalog, sql)) {
        let grouped = matches!(q.select, question::Select::Groups { .. });
        if grouped || (q.order.is_empty() && q.sides.len() > 1) {
            let at: Vec<(usize, usize)> = q.sides.iter().map(|s| tables_at[s.table]).collect();
            match read_lying(snails, &at, q, grouped, lying) {
                Err(e) if join::past_the_room(&e) => {}
                done => return done,
            }
        }
    }
    let written = pipeline::windows::written(&spelled::between_written(asked))?;
    let written = computed(&catalog, &written)?.unwrap_or(written);
    let across = relations::Across {
        snails: snails.to_vec(),
        at: &tables_at,
        catalog: &catalog,
    };
    let (rows, names, round) = relations::answer_rows(&across, &written, &[])?;
    lying.rows = Some(rows);
    lying.names = names;
    Ok(round)
}

/// A grouping, or a join's rows, read along its tables' orders: a join's rows laid out as they are
/// made, a grouping's one batch once its groups are made.
fn read_lying(
    snails: &[&Snail],
    at: &[(usize, usize)],
    mut q: question::Question,
    grouped: bool,
    lying: &mut Lying,
) -> Result<Round> {
    let (batch, round) = match (grouped, q.sides.len()) {
        (true, 1) => {
            let (snail, table) = at[0];
            q.sides[0].table = table;
            snails[snail].ask_one(&q, None)?
        }
        (true, _) => ask_join_within(snails, at, &q, true, None)?,
        (false, _) => {
            let mut lay = |b: RecordBatch| lying.take(b);
            ask_join_within(snails, at, &q, true, Some(&mut lay))?
        }
    };
    lying.take(batch)?;
    Ok(round)
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
    /// The bytes a round holds of it at most: the most rows, each at its columns' held widths, or the
    /// bytes they take in the pages it reads where those are more; none where it reads no column page.
    pub held_bytes: u64,
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
                held_bytes: p.rows_high.saturating_mul(p.row_bytes),
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
            held_bytes: part
                .rows
                .1
                .saturating_mul(row_bytes(&part.plan, table, r.columns.iter().map(|c| c.0))),
        });
    }
    Ok(Priced { tables: out })
}
