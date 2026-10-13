//! A question in PostgreSQL's SQL, read off a snail: for each table it reads, the orders that serve it,
//! each one's pages predicted from the directory before any column page is read, then the read.
//!
//! Two tables joined on keys are read one after the other: the table its own conditions narrow first,
//! the table named first where both or neither are narrowed. The other is then kept to the keys the
//! first holds.

mod answer;
mod change;
mod hash;
mod out;
mod pipeline;
mod question;
pub mod statement;
mod walk;

pub use change::Done;
pub use out::{TableOut, DDL_KEY};
pub use pipeline::{numeric_text, Value};

/// An answer's rows as values, as a pipeline reads them.
pub fn pipeline_rows(batch: &RecordBatch) -> Result<Vec<Vec<Value>>> {
    Ok(pipeline::relation_of(batch)?.rows)
}
pub use question::instant;

use crate::catalog::{Catalog, Table};
use crate::cut::PAGE;
use crate::directory::{decode_page, maps_held, read_map, PageMap};
use crate::file::{Extent, Reader, Root, Writer};
use crate::places::{shown, Sublevels};
use crate::{Error, Result};
use answer::{as_text, Answer, Cell, Dict};
use arrow::array::RecordBatch;
use hash::WordMap;
use question::{Key, Question};
use std::path::Path;
use walk::{condition_columns, next_down, ruled_out, KeySet, Plan, Sink, Want};

/// What an open snail holds between rounds unless told otherwise: PostgreSQL's default
/// `shared_buffers`.
pub const HELD_BYTES: usize = 128 << 20;

/// What the first side of a join holds of one key: how many of its rows hold it, and their cells
/// where it gives answer inputs of its own.
#[derive(Default)]
struct Kept {
    rows: u64,
    cells: Vec<Vec<Cell>>,
}

/// The first side of a join as its rows arrive: how many rows hold each key, and their cells as text
/// where the side gives answer inputs of its own.
struct First<'h> {
    held: &'h mut WordMap<Vec<i64>, Kept>,
    key: Vec<i64>,
    j: usize,
    inputs: bool,
    dicts: Vec<Dict>,
}

impl Sink for First<'_> {
    fn dictionaries(&mut self, dicts: &[Dict]) {
        self.dicts = dicts.to_vec();
    }

    fn row(&mut self, cells: &[Cell]) {
        self.key.clear();
        for c in &cells[..self.j] {
            match c {
                Cell::Int(v) => self.key.push(*v),
                _ => return,
            }
        }
        let kept = match self.held.get_mut(self.key.as_slice()) {
            Some(k) => k,
            None => self.held.entry(self.key.clone()).or_default(),
        };
        kept.rows += 1;
        if self.inputs {
            let none = None;
            let text = cells[self.j..]
                .iter()
                .enumerate()
                .map(|(i, c)| as_text(c, self.dicts.get(self.j + i).unwrap_or(&none)))
                .collect();
            kept.cells.push(text);
        }
    }
}

/// The second side of a join as its rows arrive, each met with the first side's rows of its key.
struct Second<'a> {
    held: &'a WordMap<Vec<i64>, Kept>,
    answer: &'a mut Answer,
    /// Where each of the answer's inputs comes from: (side, its place among that side's inputs).
    from: &'a [(usize, usize)],
    first: usize,
    j: usize,
    inputs: bool,
    probe: Vec<i64>,
    /// The first side's rows of the key the row admitted last.
    found: Option<&'a Kept>,
    cells: Vec<Cell>,
}

impl Sink for Second<'_> {
    fn dictionaries(&mut self, dicts: &[Dict]) {
        let mine: Vec<Dict> = self
            .from
            .iter()
            .map(|&(side, at)| {
                if side == self.first {
                    None
                } else {
                    dicts[self.j + at].clone()
                }
            })
            .collect();
        self.answer.dictionaries(&mine);
    }

    fn lead(&self) -> usize {
        self.j
    }

    fn admits(&mut self, lead: &[Cell]) -> bool {
        self.probe.clear();
        for c in lead {
            match c {
                Cell::Int(v) => self.probe.push(*v),
                _ => {
                    self.found = None;
                    return false;
                }
            }
        }
        self.found = self.held.get(self.probe.as_slice());
        self.found.is_some()
    }

    fn row(&mut self, row: &[Cell]) {
        let Some(kept) = self.found else { return };
        if !self.inputs {
            for (cell, &(_, at)) in self.cells.iter_mut().zip(self.from) {
                *cell = row[self.j + at].clone();
            }
            self.answer.add_times(&self.cells, kept.rows);
            return;
        }
        for m in &kept.cells {
            for (cell, &(side, at)) in self.cells.iter_mut().zip(self.from) {
                *cell = if side == self.first {
                    m[at].clone()
                } else {
                    row[self.j + at].clone()
                };
            }
            self.answer.add(&self.cells);
        }
    }
}

/// The tag of the pages read for one order of one table.
fn order_tag(table: usize, order: usize) -> u32 {
    (table as u32) << 16 | order as u32
}

/// A snail opened for reading: its root and its catalog read, nothing else.
pub struct Snail {
    reader: Reader,
    root: Root,
    pub catalog: Catalog,
    /// The pages the open read: the root's two copies and the catalog.
    pub pages_at_open: u64,
    rounds: u64,
    /// Where the snail is open to write, the file's one writer.
    writer: Option<Writer>,
    /// A transaction begun and not yet ended: the root it began from, and whether it wrote.
    begun: Option<(Root, bool)>,
    /// The blocks a round read a page or more of waste in, which it prunes as it ends.
    wasted: std::cell::RefCell<Vec<Wasted>>,
    /// The blocks the statement or the transaction under way has written, by table and order, each
    /// against the block it began from.
    rebased: std::collections::BTreeMap<(usize, usize), Vec<change::Rebased>>,
    /// The blocks the statement or the transaction under way has locked, by their lock bytes; and
    /// whether the statement found one written since the root it read.
    locks: std::collections::BTreeSet<u64>,
    stale: bool,
    /// The pages its commits have written into pages freed before.
    reused: u64,
    /// The catalog the statement or the transaction under way has declared.
    declared: Option<change::Declared>,
    /// The rows the scans have walked one by one, and taken in runs at once.
    walked: std::cell::Cell<u64>,
    taken: std::cell::Cell<u64>,
}

/// A block a round found a page or more of waste in: its table and order, its first key, and its
/// page map, which tells it from any block written since.
struct Wasted {
    table: usize,
    order: usize,
    first: Vec<crate::places::KeyValue>,
    child: Extent,
}

/// An order a round consulted: the column pages its directory priced the round at, and the exact
/// prediction from its page maps unless it was left unread.
#[derive(Clone, Debug)]
pub struct Consulted {
    pub order: String,
    /// The directory pages read for this order: its directory, and its page maps when predicted.
    pub directory_pages: u64,
    pub low: u64,
    pub high: u64,
    pub exact: Option<u64>,
    /// Of the exact pages, those held when the round priced the order.
    pub held: u64,
}

/// What a round read, and what the directory said it would read before it did.
#[derive(Clone, Debug, Default)]
pub struct Round {
    /// The order read, or for a join the order read of each table, the first table read first.
    pub order: String,
    /// The orders that hold every column the question reads; none are listed for a join.
    pub served: Vec<String>,
    pub consulted: Vec<Consulted>,
    /// The pages read from the file as the round began, after the first round of an open snail: the
    /// root's two copies, and the catalog on a new generation. The first round's are the open's.
    pub pages_at_start: u64,
    /// The directory pages the round touched, and of them, those found held.
    pub directory_pages: u64,
    pub directory_pages_held: u64,
    /// The fewest and the most column pages the order's directory allowed: at its top, and at its
    /// blocks before their page maps.
    pub column_pages_top_low: u64,
    pub column_pages_top_high: u64,
    pub column_pages_low: u64,
    pub column_pages_high: u64,
    pub column_pages_predicted: u64,
    /// Of the column pages predicted, those held when the round chose its order, which it keeps until
    /// it ends; the rest it reads from the file.
    pub column_pages_held: u64,
    /// The column pages read from the file.
    pub column_pages_read: u64,
    /// The entries the round looked at, at every level of the order's directory: none of whose rows
    /// the question holds, some, or every one.
    pub entries: u64,
    pub entries_none: u64,
    pub entries_may: u64,
    pub entries_must: u64,
    /// Blocks whose pages were read; the rest were answered from their counts or skipped.
    pub blocks_read: u64,
    pub rows: u64,
    /// On a snail open to write, the blocks the round pruned as it ended, and the pages it wrote.
    pub blocks_pruned: u64,
    pub pages_written: u64,
    /// For a question read as a pipeline, the most rows it held at once as the table's rows came:
    /// the chunk read, each step's chunk, groups, running folds and rows held to sort.
    pub rows_held: u64,
    /// The rows the scan walked one by one, and those it handed on in runs at once.
    pub rows_walked: u64,
    pub rows_taken: u64,
}

/// One table's part of a round: the plan read, the orders that served it, and what each said.
struct Part {
    plan: Plan,
    served: Vec<String>,
    consulted: Vec<Consulted>,
}

impl Snail {
    /// Opens a snail. Built `standalone`, it holds `HELD_BYTES` of the pages its rounds read.
    pub fn open(path: &Path) -> Result<Snail> {
        #[cfg(feature = "standalone")]
        return Snail::open_holding(path, HELD_BYTES);
        #[cfg(not(feature = "standalone"))]
        Snail::opened(Reader::open(path)?)
    }

    /// Opens a snail that holds up to `bytes` of the pages its rounds read, between rounds.
    #[cfg(feature = "standalone")]
    pub fn open_holding(path: &Path, bytes: usize) -> Result<Snail> {
        Snail::opened(Reader::holding(path, bytes / PAGE)?)
    }

    fn opened(reader: Reader) -> Result<Snail> {
        let root = reader.root()?;
        let catalog = Snail::catalog(&reader, &root)?;
        let pages_at_open = reader.pages_read();
        Ok(Snail {
            reader,
            root,
            catalog,
            pages_at_open,
            rounds: 0,
            writer: None,
            begun: None,
            wasted: Default::default(),
            walked: Default::default(),
            taken: Default::default(),
            rebased: Default::default(),
            locks: Default::default(),
            stale: false,
            reused: 0,
            declared: None,
        })
    }

    /// Opens a snail to write, as its one writer, holding the pages its rounds read up to the size an
    /// open snail holds. Built without `standalone`, as inside PostgreSQL, a snail writes nothing:
    /// its commit would have to be PostgreSQL's.
    pub fn open_writing(path: &Path) -> Result<Snail> {
        #[cfg(feature = "standalone")]
        {
            let (writer, _) = Writer::open(path)?;
            let mut snail = Snail::open(path)?;
            snail.writer = Some(writer);
            Ok(snail)
        }
        #[cfg(not(feature = "standalone"))]
        {
            let _ = path;
            Err(Error::new(
                "a snail built without `standalone` writes nothing: inside PostgreSQL its commit \
                 would have to be PostgreSQL's",
            ))
        }
    }

    fn catalog(reader: &Reader, root: &Root) -> Result<Catalog> {
        let ddl = String::from_utf8(reader.read_extent(root.catalog)?)
            .map_err(|_| Error::new("the catalog is not UTF-8"))?;
        Catalog::parse(&ddl)
    }

    /// Begins a round after the first: the root read afresh, and the catalog read again if the file
    /// has moved to a new generation. The pages it read from the file.
    fn renew(&mut self) -> Result<u64> {
        let before = self.reader.pages_read();
        let root = self.reader.begin_round()?;
        if root.generation != self.root.generation {
            self.catalog = Snail::catalog(&self.reader, &root)?;
        }
        self.root = root;
        Ok(self.reader.pages_read() - before)
    }

    /// The generation of the root the snail reads.
    pub fn generation(&self) -> u64 {
        self.root.generation
    }

    /// The pages read from the file.
    pub fn pages_read(&self) -> u64 {
        self.reader.pages_read()
    }

    /// The pages found held, rather than read from the file.
    pub fn pages_held(&self) -> u64 {
        self.reader.pages_held()
    }

    /// The pages touched: read from the file, or found held.
    fn touched(&self) -> u64 {
        self.reader.pages_read() + self.reader.pages_held()
    }

    /// One table's round: every order that serves it priced from its top, each walked one level
    /// further down while another order's bounds overlap its own, and the one whose exact pages are
    /// fewest read.
    fn part(&self, want: &Want, order: Option<&str>) -> Result<Part> {
        let table = want.table;
        let mut needed = condition_columns(want.side);
        needed.extend(want.columns.iter().copied());
        if let Some(k) = want.keys {
            needed.extend(k.columns.iter().copied());
        }
        let served: Vec<usize> = (0..table.orders.len())
            .filter(|&o| needed.iter().all(|&c| table.orders[o].slot(c).is_some()))
            .collect();
        if served.is_empty() {
            return Err(Error::new(format!(
                "no order of {} holds every column the question reads",
                table.name
            )));
        }
        let candidates: Vec<usize> = match order {
            Some(n) => {
                let o = table
                    .orders
                    .iter()
                    .position(|x| x.name == n)
                    .ok_or_else(|| Error::new(format!("{} has no order {n}", table.name)))?;
                if !served.contains(&o) {
                    return Err(Error::new(format!(
                        "{n} does not hold every column the question reads"
                    )));
                }
                vec![o]
            }
            None => served.clone(),
        };
        let mut walks = Vec::new();
        for o in candidates {
            walks.push(self.start(o, want)?);
        }
        let mut out = vec![false; walks.len()];
        loop {
            let live: Vec<usize> = (0..walks.len()).filter(|&i| !out[i]).collect();
            // A held page is read from no file, so an order's fewest pages from the file are its fewest
            // less every column page held for it.
            let ranges: Vec<(u64, u64)> = live
                .iter()
                .map(|&i| {
                    let held = self
                        .reader
                        .held_under(order_tag(want.side.table, walks[i].order));
                    (walks[i].low.saturating_sub(held), walks[i].high)
                })
                .collect();
            // The directory pages an order reads next from the file count too: an order is out whose
            // fewest pages so counted are more than the most of an order walked to its blocks.
            let totals: Vec<(u64, u64)> = live
                .iter()
                .zip(&ranges)
                .map(|(&i, r)| {
                    let w = &walks[i];
                    let below = w.below().count() as u64 - self.reader.held_among(w.below());
                    let most = if w.level == 1 { below + r.1 } else { u64::MAX };
                    (below + r.0, most)
                })
                .collect();
            for (k, (r, t)) in ruled_out(&ranges)
                .into_iter()
                .zip(ruled_out(&totals))
                .enumerate()
            {
                out[live[k]] = r || t;
            }
            // The order whose fewest pages are fewest reads one level further down, alone.
            let next = next_down(
                live.iter()
                    .zip(&totals)
                    .filter(|(&i, _)| !out[i] && walks[i].level > 1 && walks[i].is_open())
                    .map(|(&i, t)| (i, t.0)),
            );
            match next {
                Some(i) => self.descend(&mut walks[i], want)?,
                None => break,
            }
        }
        let mut consulted = Vec::new();
        let mut plans = Vec::new();
        for (walk, out) in walks.into_iter().zip(out) {
            let name = table.orders[walk.order].name.clone();
            if out {
                consulted.push(Consulted {
                    order: name,
                    directory_pages: walk.directory_pages,
                    low: walk.low,
                    high: walk.high,
                    exact: None,
                    held: 0,
                });
                continue;
            }
            let plan = self.predict(walk, want)?;
            let held = self.reader.held_among(plan.disk_pages());
            consulted.push(Consulted {
                order: name,
                directory_pages: plan.walk.directory_pages,
                low: plan.walk.low,
                high: plan.walk.high,
                exact: Some(plan.column_pages),
                held,
            });
            plans.push((plan, held));
        }
        // The fewest pages from the file win; then the fewest touched, then the fewest directory pages.
        let best = (0..plans.len())
            .min_by_key(|&i| {
                let (p, held) = &plans[i];
                (
                    p.column_pages - held,
                    p.column_pages,
                    p.walk.directory_pages,
                )
            })
            .unwrap();
        let (plan, _) = plans.swap_remove(best);
        Ok(Part {
            plan,
            served: served
                .iter()
                .map(|&o| table.orders[o].name.clone())
                .collect(),
            consulted,
        })
    }

    /// Answers a question, along the order named or the one whose pages are fewest: one round.
    pub fn ask(&mut self, sql: &str, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        self.ask_with(sql, order, &[])
    }

    /// Answers a question with its parameters, `$1` and on, given as text: one round.
    pub fn ask_with(
        &mut self,
        sql: &str,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let pages_at_start = if self.rounds > 0 && self.begun.is_none() {
            self.renew()?
        } else {
            0
        };
        self.rounds += 1;
        self.wasted.borrow_mut().clear();
        let (batch, mut round) = self.answer_with(sql, order, params)?;
        round.pages_at_start = pages_at_start;
        let wasted = self.wasted.take();
        if !wasted.is_empty() {
            (round.blocks_pruned, round.pages_written) = self.prune(wasted)?;
        }
        Ok((batch, round))
    }

    /// A question answered on the file as this round reads it.
    fn answer(&self, sql: &str, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        self.answer_with(sql, order, &[])
    }

    fn answer_with(
        &self,
        sql: &str,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, sql)?;
        if let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() {
            if pipeline::is_pipeline(q) {
                if order.is_some() {
                    return Err(Error::new("a pipeline reads its table along its own order"));
                }
                return self.ask_pipeline(q, params);
            }
        }
        if !params.is_empty() {
            return Err(Error::new("parameters are read only in a pipeline"));
        }
        let q = question::parse(&self.catalog, sql)?;
        match q.sides.len() {
            1 => self.ask_one(&q, order),
            _ if order.is_some() => Err(Error::new("a join chooses its orders itself")),
            _ => self.ask_join(&q),
        }
    }

    /// A question read as a pipeline: its one table read whole along its own order, then its steps.
    fn ask_pipeline(
        &self,
        q: &sqlparser::ast::Query,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let name = pipeline::table_named(q)?;
        let table = self
            .catalog
            .tables
            .iter()
            .find(|t| t.name == name || t.name.rsplit('.').next() == Some(name.as_str()))
            .ok_or_else(|| Error::new(format!("{name} is not in the snail")))?;
        let order = table
            .orders
            .iter()
            .find(|o| o.table_order)
            .ok_or_else(|| Error::new(format!("{} has no order of its own", table.name)))?;
        let columns = table
            .columns
            .iter()
            .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let base = question::parse(
            &self.catalog,
            &format!("SELECT {columns} FROM {}", table.name),
        )?;
        // The table's order carries its windows where it is one level, unique: its key's column.
        let along = match (order.unique, order.levels.as_slice()) {
            (true, [crate::catalog::Level::Column(c)]) => Some(*c),
            _ => None,
        };
        let names = table.columns.iter().map(|c| c.name.clone()).collect();
        let mut stream = pipeline::Stream::new(q, &table.name, names, along, params)?;
        // Each block's rows go to the pipeline as the scan reads them, in the order.
        let mut blocks = Blocks {
            table,
            q: &base,
            answer: Answer::new(&base.select),
            feed: |batch: RecordBatch| {
                let mut rel = pipeline::relation_of(&batch)?;
                if let Some(c) = along {
                    pipeline::in_order(&mut rel, c)?;
                }
                stream.feed(rel)
            },
        };
        let (mut round, _, _) = self.scan_one(&base, Some(&order.name), &mut blocks)?;
        drop(blocks);
        round.rows_held = stream.most_held() as u64;
        let out = stream.finish()?;
        let batch = pipeline::batch_of(&out)?;
        round.rows = batch.num_rows() as u64;
        Ok((batch, round))
    }

    fn ask_one(&self, q: &Question, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        let table = &self.catalog.tables[q.sides[0].table];
        let mut answer = Answer::new(&q.select);
        let (mut round, counted, months) = self.scan_one(q, order, &mut answer)?;
        answer.counted(counted, &months, q.select.countable());
        let batch = answer.finish(&[table], &q.select, &q.order)?;
        round.rows = batch.num_rows() as u64;
        Ok((batch, round))
    }

    /// A question on one table read along an order into a sink: the round, and what the directory
    /// counted, in all and by month.
    fn scan_one(
        &self,
        q: &Question,
        order: Option<&str>,
        sink: &mut dyn Sink,
    ) -> Result<(Round, u64, [u64; 12])> {
        let table = &self.catalog.tables[q.sides[0].table];
        let inputs = q.select.inputs();
        let columns: Vec<(usize, bool)> = inputs
            .iter()
            .map(|k| (k.col().column, matches!(k, Key::Month(_))))
            .collect();
        let want = Want {
            table,
            side: &q.sides[0],
            columns: columns.iter().map(|c| c.0).collect(),
            countable: q.select.countable(),
            keys: None,
        };
        let (before, held_before) = (self.touched(), self.pages_held());
        let part = self.part(&want, order)?;
        let directory_pages = self.touched() - before;
        let directory_pages_held = self.pages_held() - held_before;
        let column_pages_held = self.reader.keep_for_round(part.plan.disk_pages());
        let mid = self.reader.pages_read();
        let (walked, taken) = (self.walked.get(), self.taken.get());
        self.scan(&part.plan, &want, &columns, sink)?;
        let column_pages_read = self.reader.pages_read() - mid;
        let w = &part.plan.walk;
        let round = Round {
            order: table.orders[w.order].name.clone(),
            served: part.served.clone(),
            consulted: part.consulted.clone(),
            pages_at_start: 0,
            directory_pages,
            directory_pages_held,
            column_pages_top_low: w.top.0,
            column_pages_top_high: w.top.1,
            column_pages_low: w.low,
            column_pages_high: w.high,
            column_pages_predicted: part.plan.column_pages,
            column_pages_held,
            column_pages_read,
            entries: w.statuses.0 + w.statuses.1 + w.statuses.2,
            entries_none: w.statuses.0,
            entries_may: w.statuses.1,
            entries_must: w.statuses.2,
            blocks_read: part.plan.blocks_read(),
            rows: 0,
            blocks_pruned: 0,
            pages_written: 0,
            rows_held: 0,
            rows_walked: self.walked.get() - walked,
            rows_taken: self.taken.get() - taken,
        };
        Ok((round, w.counted, w.counted_months))
    }

    fn ask_join(&self, q: &Question) -> Result<(RecordBatch, Round)> {
        let (walked, taken) = (self.walked.get(), self.taken.get());
        let first = if q.sides[1].conditioned() && !q.sides[0].conditioned() {
            1
        } else {
            0
        };
        let second = 1 - first;
        let join_columns = |side: usize| -> Vec<usize> {
            q.join
                .iter()
                .map(|&(a, b)| if side == 0 { a } else { b })
                .collect()
        };
        let tables: Vec<&Table> = q
            .sides
            .iter()
            .map(|s| &self.catalog.tables[s.table])
            .collect();
        let inputs = q.select.inputs();
        // Each side's scan returns the join's columns, then the answer's inputs it holds.
        let scan_columns = |side: usize| -> Vec<(usize, bool)> {
            join_columns(side)
                .into_iter()
                .map(|c| (c, false))
                .chain(
                    inputs
                        .iter()
                        .filter(|k| k.col().side == side)
                        .map(|k| (k.col().column, matches!(k, Key::Month(_)))),
                )
                .collect()
        };
        let j = q.join.len();

        let a_columns = scan_columns(first);
        let a_want = Want {
            table: tables[first],
            side: &q.sides[first],
            columns: a_columns.iter().map(|c| c.0).collect(),
            countable: None,
            keys: None,
        };
        let (before, held_before) = (self.touched(), self.pages_held());
        let a = self.part(&a_want, None)?;
        let mut directory_pages = self.touched() - before;
        let mut directory_pages_held = self.pages_held() - held_before;
        let mut column_pages_held = self.reader.keep_for_round(a.plan.disk_pages());
        let mid = self.reader.pages_read();
        // Each key the first side holds: how many rows hold it, and their cells where the first side
        // gives the answer inputs of its own.
        let a_inputs = a_columns.len() > j;
        let mut held: WordMap<Vec<i64>, Kept> = WordMap::default();
        let mut sink = First {
            held: &mut held,
            key: Vec::with_capacity(j),
            j,
            inputs: a_inputs,
            dicts: Vec::new(),
        };
        self.scan(&a.plan, &a_want, &a_columns, &mut sink)?;
        let mut column_pages_read = self.reader.pages_read() - mid;

        let keys = KeySet::new(join_columns(second), held.keys());
        let b_columns = scan_columns(second);
        let b_want = Want {
            table: tables[second],
            side: &q.sides[second],
            columns: b_columns.iter().map(|c| c.0).collect(),
            countable: None,
            keys: Some(&keys),
        };
        let (before, held_before) = (self.touched(), self.pages_held());
        let b = self.part(&b_want, None)?;
        directory_pages += self.touched() - before;
        directory_pages_held += self.pages_held() - held_before;
        column_pages_held += self.reader.keep_for_round(b.plan.disk_pages());

        // Where each of the answer's inputs comes from: (side, its place among that side's cells).
        let mut from = Vec::with_capacity(inputs.len());
        let mut seen = [0usize; 2];
        for k in &inputs {
            let side = k.col().side;
            from.push((side, seen[side]));
            seen[side] += 1;
        }
        let mut answer = Answer::new(&q.select);
        let mid = self.reader.pages_read();
        let mut sink = Second {
            held: &held,
            answer: &mut answer,
            from: &from,
            first,
            j,
            inputs: a_inputs,
            probe: Vec::with_capacity(j),
            found: None,
            cells: vec![Cell::Null; inputs.len()],
        };
        self.scan(&b.plan, &b_want, &b_columns, &mut sink)?;
        column_pages_read += self.reader.pages_read() - mid;
        let batch = answer.finish(&tables, &q.select, &q.order)?;
        let (wa, wb) = (&a.plan.walk, &b.plan.walk);
        let entries = |w: &walk::Walk| w.statuses.0 + w.statuses.1 + w.statuses.2;
        let round = Round {
            order: format!(
                "{}+{}",
                tables[first].orders[wa.order].name, tables[second].orders[wb.order].name
            ),
            served: Vec::new(),
            consulted: a.consulted.iter().chain(&b.consulted).cloned().collect(),
            pages_at_start: 0,
            directory_pages,
            directory_pages_held,
            column_pages_top_low: wa.top.0 + wb.top.0,
            column_pages_top_high: wa.top.1 + wb.top.1,
            column_pages_low: wa.low + wb.low,
            column_pages_high: wa.high + wb.high,
            column_pages_predicted: a.plan.column_pages + b.plan.column_pages,
            column_pages_held,
            column_pages_read,
            entries: entries(wa) + entries(wb),
            entries_none: wa.statuses.0 + wb.statuses.0,
            entries_may: wa.statuses.1 + wb.statuses.1,
            entries_must: wa.statuses.2 + wb.statuses.2,
            blocks_read: a.plan.blocks_read() + b.plan.blocks_read(),
            rows: batch.num_rows() as u64,
            blocks_pruned: 0,
            pages_written: 0,
            rows_held: 0,
            rows_walked: self.walked.get() - walked,
            rows_taken: self.taken.get() - taken,
        };
        Ok((batch, round))
    }

    /// Every block of every order: what the directory holds of it, column by column.
    /// Every page of the file past the root's two, by what holds it at the snail's generation: named
    /// by the root, free, or waiting to be freed. Refused where a page is held twice, or by nothing.
    pub fn census(&self) -> Result<Census> {
        let mut owner: Vec<u8> = vec![0; self.root.pages as usize];
        let mut mark = |e: Extent, as_: u8| -> Result<()> {
            for n in e.start..e.start + e.pages().max(1) {
                let slot = owner
                    .get_mut(n as usize)
                    .ok_or_else(|| Error::new(format!("page {n} lies past the file's last")))?;
                if *slot != 0 {
                    return Err(Error::new(format!("page {n} is held twice")));
                }
                *slot = as_;
            }
            Ok(())
        };
        mark(self.root.catalog, 1)?;
        // Each page of packed page maps: how many maps it holds, and how many entries name one.
        let mut shared: std::collections::BTreeMap<u64, (u32, u32)> = Default::default();
        for table in &self.catalog.tables {
            for (oi, order) in table.orders.iter().enumerate() {
                let (_, _, top) = self
                    .root
                    .orders
                    .iter()
                    .find(|(t, o, _)| *t == table.name && *o == order.name)
                    .ok_or_else(|| Error::new("the root names no directory for an order"))?;
                mark(*top, 1)?;
                let (head, mut level, _) = self.top(table, oi)?;
                for _ in 1..head.height {
                    let mut below = Vec::new();
                    for e in &level {
                        mark(e.child, 1)?;
                        below.extend(decode_page(&self.reader.read_extent(e.child)?, &head)?);
                    }
                    level = below;
                }
                for b in &level {
                    let (run, at) = b.map_run();
                    let buf = self.reader.read_extent(run)?;
                    match maps_held(&buf, at) {
                        1 => mark(run, 1)?,
                        n => {
                            let s = shared.entry(run.start).or_insert((n, 0));
                            if s.1 == 0 {
                                mark(
                                    Extent {
                                        start: run.start,
                                        bytes: PAGE as u64,
                                    },
                                    1,
                                )?;
                            }
                            s.1 += 1;
                        }
                    }
                    let map = PageMap::decode(&buf[at..], &head.level_text)?;
                    for c in &map.columns {
                        if let Some(d) = c.dictionary {
                            mark(d, 1)?;
                        }
                        for p in &c.pages {
                            mark(p.at, 1)?;
                        }
                    }
                }
            }
        }
        let (runs, spent) = if self.root.free.bytes > 0 {
            mark(self.root.free, 1)?;
            crate::file::decode_free(
                &self.reader.read_extent(self.root.free)?,
                self.reader.format(),
            )?
        } else {
            (Vec::new(), Vec::new())
        };
        // Every page of packed maps names each map it holds once: by a block's entry, or as spent.
        let spent: std::collections::BTreeMap<u64, u32> = spent.into_iter().collect();
        for (page, &(held, named)) in &shared {
            let gone = spent.get(page).copied().unwrap_or(0);
            if named + gone != held {
                return Err(Error::new(format!(
                    "page {page} holds {held} page maps: {named} named and {gone} spent"
                )));
            }
        }
        if let Some(page) = spent.keys().find(|p| !shared.contains_key(p)) {
            return Err(Error::new(format!(
                "page {page} is spent, and holds no page map named"
            )));
        }
        {
            for r in runs {
                mark(
                    Extent {
                        start: r.start,
                        bytes: r.pages * PAGE as u64,
                    },
                    if r.tag == 0 { 2 } else { 3 },
                )?;
            }
        }
        if let Some(n) = owner[2..].iter().position(|&o| o == 0) {
            return Err(Error::new(format!("page {} is held by nothing", n + 2)));
        }
        let count = |as_: u8| owner.iter().filter(|&&o| o == as_).count() as u64;
        Ok(Census {
            named: count(1),
            free: count(2),
            waiting: count(3),
        })
    }

    pub fn blocks(&self) -> Result<Vec<BlockRow>> {
        let mut out = Vec::new();
        for table in &self.catalog.tables {
            for (oi, order) in table.orders.iter().enumerate() {
                let (head, mut level, _) = self.top(table, oi)?;
                for _ in 1..head.height {
                    let mut below = Vec::new();
                    for e in &level {
                        below.extend(decode_page(&self.reader.read_extent(e.child)?, &head)?);
                    }
                    level = below;
                }
                let sublevels = Sublevels::new(order, table, &head.level_text);
                for (bi, b) in level.iter().enumerate() {
                    let place: Vec<i64> = (0..b.depth as usize)
                        .map(|r| sublevels.value(&b.first, r))
                        .collect();
                    let map = read_map(&self.reader, b, &head.level_text)?;
                    let (map_run, _) = b.map_run();
                    for (s, &c) in order.columns.iter().enumerate() {
                        let cp = &map.columns[s];
                        out.push(BlockRow {
                            table: table.name.clone(),
                            order: order.name.clone(),
                            block: bi as u64,
                            depth: b.depth as u64,
                            place: format!("{:?}", place),
                            first: shown(&b.first),
                            last: shown(&b.last),
                            rows: b.rows as u64,
                            map_pages: map_run.pages(),
                            column: table.columns[c].name.clone(),
                            k: head.units[s] as u64,
                            dictionary_pages: cp.dictionary.map_or(0, Extent::pages),
                            data_pages: cp.pages.len() as u64,
                            disk_pages: cp.dictionary.map_or(0, Extent::pages)
                                + cp.pages.iter().map(|p| p.at.pages()).sum::<u64>(),
                            pages_listed: b.columns[s].pages as u64,
                            bytes: head
                                .text_slots
                                .iter()
                                .position(|&x| x as usize == s)
                                .map(|t| b.bytes[t] as u64),
                            nulls: b.columns[s].nulls as u64,
                            dead: b.dead as u64,
                            dead_marked: map.dead.len() as u64,
                            chains: map.chains.len() as u64,
                            files: {
                                let span = self.root.span.max(1);
                                let mut files = std::collections::BTreeSet::new();
                                for e in [map_run]
                                    .into_iter()
                                    .chain(cp.dictionary)
                                    .chain(cp.pages.iter().map(|p| p.at))
                                {
                                    let last = e.start + e.pages().max(1) - 1;
                                    files.extend(e.start / span..=last / span);
                                }
                                files.len() as u64
                            },
                        });
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Hands the rows of each block a scan reads to a pipeline, one answer a block.
struct Blocks<'a, F: FnMut(RecordBatch) -> Result<()>> {
    table: &'a Table,
    q: &'a Question,
    answer: Answer,
    feed: F,
}

impl<F: FnMut(RecordBatch) -> Result<()>> Sink for Blocks<'_, F> {
    fn dictionaries(&mut self, dicts: &[Dict]) {
        self.answer.dictionaries(dicts);
    }

    fn row(&mut self, cells: &[Cell]) {
        self.answer.add(cells);
    }

    fn block_end(&mut self) -> Result<()> {
        // The scan sends each block's own dictionaries before the block's first row.
        let done = std::mem::replace(&mut self.answer, Answer::new(&self.q.select));
        let batch = done.finish(&[self.table], &self.q.select, &self.q.order)?;
        (self.feed)(batch)
    }
}

/// A file's pages past the root's two, by what holds them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Census {
    pub named: u64,
    pub free: u64,
    pub waiting: u64,
}

/// One block's column, as the directory holds it.
#[derive(Clone, Debug)]
pub struct BlockRow {
    pub table: String,
    pub order: String,
    pub block: u64,
    pub depth: u64,
    pub place: String,
    pub first: String,
    pub last: String,
    pub rows: u64,
    pub map_pages: u64,
    pub column: String,
    pub k: u64,
    pub dictionary_pages: u64,
    pub data_pages: u64,
    pub disk_pages: u64,
    /// The column's disk pages as the order's directory lists them.
    pub pages_listed: u64,
    /// The bytes of a text column's values present, as the directory keeps them.
    pub bytes: Option<u64>,
    /// The column's NULLs among the block's live rows, as the directory counts them.
    pub nulls: u64,
    /// The block's dead rows, as its entry counts them and as its page map marks them; and the dead
    /// rows its page map chains to the rows that replaced them.
    pub dead: u64,
    pub dead_marked: u64,
    pub chains: u64,
    /// The files the block's page map and the column's pages lie in.
    pub files: u64,
}

#[cfg(test)]
mod tests {
    use super::order_tag;

    #[test]
    fn each_order_of_each_table_has_a_tag_of_its_own() {
        let tags: Vec<u32> = (0..3)
            .flat_map(|t| (0..4).map(move |o| order_tag(t, o)))
            .collect();
        let mut distinct = tags.clone();
        distinct.sort();
        distinct.dedup();
        assert_eq!(distinct.len(), tags.len());
    }
}
