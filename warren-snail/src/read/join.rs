//! A question on several tables. The tables are read one after another, from the question's scopes:
//! first each table whose own conditions fix places on one of its orders, the first table as
//! written where none does; then, along the chain, each table joined on its keys to the table read
//! just before it, kept to the keys that table holds, and to those of every other table read before
//! it that it meets so. A table an outer join keeps is never kept to another's keys; the table it may
//! leave NULL is, to the keys of the tables its ON meets it from. A table met only by inequalities
//! is read by its own conditions.
//!
//! Each table's pages are predicted from its directory once the keys it is kept to are known, before
//! any of its own column pages are read. The rows then meet in memory: the tables no outer join may
//! leave NULL first, on their keys and their inequalities, then each outer join as written, then what
//! WHERE reads on a table an outer join may leave NULL.

use super::answer::{as_text, Answer, Cell, Dict};
use super::hash::WordMap;
use super::question::{Cmp, Key, Meet, Piece, Question, Test};
use super::walk::{KeySet, Sink, Want};
use super::{Round, Snail};
use crate::catalog::{Column, ColumnType, Level, Table};
use crate::places::KeyValue;
use crate::Result;
use arrow::array::RecordBatch;

/// A value as a join meets it: a number's key in 128 bits, or the bytes a value is keyed by.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Part {
    Num(i128),
    Bytes(Box<[u8]>),
}

/// The value a cell of a column of type `ty` meets another by; none for a NULL, which meets none.
fn part_of(c: &Cell, ty: ColumnType) -> Option<Part> {
    Some(match c {
        Cell::Null | Cell::Code(_) => return None,
        Cell::Int(v) => Part::Num(i128::from(*v)),
        Cell::Wide(v) => Part::Num(*v),
        Cell::Float(f) => Part::Num(i128::from(f.key)),
        Cell::Interval(v) => Part::Num(v.0.total()),
        Cell::Text(t) => Part::Bytes(crate::strings::key_bytes(ty, t.0.data()).into()),
        Cell::Keyed(k) => Part::Bytes(k.key.clone()),
        Cell::Classed(c) => Part::Bytes(c.class.0.data().into()),
        Cell::Composed(x) => Part::Bytes(x.key.clone()),
    })
}

/// A cell of a column of type `from` as a level of column `to` keys it; none where no value of the
/// level is it.
fn level_value(c: &Cell, from: ColumnType, to: &Column) -> Option<KeyValue> {
    Some(match part_of(c, from)? {
        Part::Num(v) if to.ty.is_wide() => KeyValue::Wide(v),
        Part::Num(v) => KeyValue::Int(i64::try_from(v).ok()?),
        Part::Bytes(_) if matches!(c, Cell::Composed(_)) => match c {
            Cell::Composed(x) => KeyValue::Text(x.hex.clone()),
            _ => unreachable!(),
        },
        Part::Bytes(b) => crate::places::column_byte_key(to, &b),
    })
}

/// Whether a cell of a column of type `ty` passes a condition on that column.
fn passes(p: &Piece, c: &Cell, ty: ColumnType) -> bool {
    match p {
        Piece::Empty => false,
        Piece::Null(_, null) => matches!(c, Cell::Null) == *null,
        Piece::Cond(cond) => cond.holds(match part_of(c, ty) {
            Some(Part::Num(v)) => Some(v),
            _ => None,
        }),
        Piece::Text(t) => match part_of(c, ty) {
            Some(Part::Bytes(b)) => t.holds(&b),
            _ => false,
        },
        Piece::Op(_) => unreachable!("an operator's condition is read on its own table"),
    }
}

/// Whether two cells pass a comparison: neither NULL, and their keys compared so.
fn compares(op: Cmp, a: &Cell, ta: ColumnType, b: &Cell, tb: ColumnType) -> bool {
    match (part_of(a, ta), part_of(b, tb)) {
        (Some(x), Some(y)) => op.holds(x.cmp(&y)),
        _ => false,
    }
}

/// No row: a table an outer join leaves NULL.
const NONE: u32 = u32::MAX;

/// One table's rows as the round read them: the cells of the columns the join reads of it, text as
/// text, and how many rows of the table each stands for.
#[derive(Default)]
struct Held {
    cells: Vec<Vec<Cell>>,
    weights: Vec<u64>,
}

/// The keys of a table read before that a table is kept to: where its columns lie among those this
/// table's scan returns, and each key's values.
struct Filter {
    at: Vec<usize>,
    types: Vec<ColumnType>,
    keys: WordMap<Vec<Part>, ()>,
}

/// A table's rows as they arrive, each kept where it holds a key of every table it is kept to: as
/// its cells, or, where none of its cells is the answer's, as one row of its cells for each run of
/// equal ones, counted.
struct Gather<'a> {
    held: &'a mut Held,
    index: Option<WordMap<Vec<Cell>, usize>>,
    filters: &'a [Filter],
    lead: usize,
    dicts: Vec<Dict>,
    probe: Vec<Part>,
}

impl Gather<'_> {
    fn text(&self, at: usize, c: &Cell) -> Cell {
        as_text(c, self.dicts.get(at).unwrap_or(&None))
    }
}

impl Sink for Gather<'_> {
    fn dictionaries(&mut self, dicts: &[Dict]) {
        self.dicts = dicts.to_vec();
    }

    fn lead(&self) -> usize {
        self.lead
    }

    fn admits(&mut self, lead: &[Cell]) -> bool {
        for f in self.filters {
            self.probe.clear();
            for (&at, &ty) in f.at.iter().zip(&f.types) {
                match part_of(&self.text(at, &lead[at]), ty) {
                    Some(p) => self.probe.push(p),
                    None => return false,
                }
            }
            if !f.keys.contains_key(self.probe.as_slice()) {
                return false;
            }
        }
        true
    }

    fn row(&mut self, cells: &[Cell]) {
        let row: Vec<Cell> = cells
            .iter()
            .enumerate()
            .map(|(i, c)| self.text(i, c))
            .collect();
        match &mut self.index {
            Some(index) => match index.get(&row) {
                Some(&i) => self.held.weights[i] += 1,
                None => {
                    index.insert(row.clone(), self.held.cells.len());
                    self.held.cells.push(row);
                    self.held.weights.push(1);
                }
            },
            None => {
                self.held.cells.push(row);
                self.held.weights.push(1);
            }
        }
    }
}

/// The rows met so far: for each, the row of each table it joins, or `NONE`, laid flat, and how many
/// rows of the answer it stands for.
struct Joined {
    width: usize,
    rows: Vec<u32>,
    weights: Vec<u64>,
}

impl Joined {
    fn len(&self) -> usize {
        self.weights.len()
    }

    fn row(&self, i: usize) -> &[u32] {
        &self.rows[i * self.width..(i + 1) * self.width]
    }

    fn push(&mut self, row: &[u32], weight: u64) {
        self.rows.extend_from_slice(row);
        self.weights.push(weight);
    }
}

/// What the round reads of each table: the columns its scan returns, its rows once read, and its
/// columns' types.
struct Tables<'a> {
    tables: Vec<&'a Table>,
    columns: Vec<Vec<(usize, bool)>>,
    held: Vec<Held>,
}

static NULL: Cell = Cell::Null;

impl Tables<'_> {
    fn at(&self, side: usize, column: usize, month: bool) -> usize {
        self.columns[side]
            .iter()
            .position(|&c| c == (column, month))
            .expect("a column the join reads is scanned")
    }

    fn ty(&self, side: usize, column: usize) -> ColumnType {
        self.tables[side].columns[column].ty
    }

    /// The cell of a column of a table in a row met, NULL where the table stands NULL.
    fn cell(&self, row: &[u32], side: usize, column: usize, month: bool) -> &Cell {
        match row[side] {
            NONE => &NULL,
            r => &self.held[side].cells[r as usize][self.at(side, column, month)],
        }
    }

    fn test(&self, t: &Test, row: &[u32]) -> bool {
        match t {
            Test::One(s, p) => match p.column() {
                Some((c, month)) => passes(p, self.cell(row, *s, c, month), self.ty(*s, c)),
                None => passes(p, &NULL, ColumnType::Int8),
            },
            Test::Two(m) => self.meets(m, row),
        }
    }

    fn meets(&self, m: &Meet, row: &[u32]) -> bool {
        compares(
            m.op,
            self.cell(row, m.a.side, m.a.column, false),
            self.ty(m.a.side, m.a.column),
            self.cell(row, m.b.side, m.b.column, false),
            self.ty(m.b.side, m.b.column),
        )
    }

    /// The values a row met holds on these columns, none where one of them is NULL.
    fn key(&self, row: &[u32], cols: &[super::question::Col]) -> Option<Vec<Part>> {
        cols.iter()
            .map(|c| {
                part_of(
                    self.cell(row, c.side, c.column, false),
                    self.ty(c.side, c.column),
                )
            })
            .collect()
    }

    /// Table `side`'s rows by their values on these of its columns.
    fn by_key(&self, side: usize, cols: &[usize]) -> WordMap<Vec<Part>, Vec<u32>> {
        let mut map: WordMap<Vec<Part>, Vec<u32>> = WordMap::default();
        let at: Vec<usize> = cols.iter().map(|&c| self.at(side, c, false)).collect();
        for (r, cells) in self.held[side].cells.iter().enumerate() {
            let key: Option<Vec<Part>> = cols
                .iter()
                .zip(&at)
                .map(|(&c, &a)| part_of(&cells[a], self.ty(side, c)))
                .collect();
            if let Some(k) = key {
                map.entry(k).or_default().push(r as u32);
            }
        }
        map
    }

    /// The rows met joined with table `side`'s on `meets`, each written from its column, and
    /// `tests`; where `outer`, a row met that joins none is kept with `side` NULL.
    fn join(&self, j: &Joined, side: usize, meets: &[Meet], tests: &[Test], outer: bool) -> Joined {
        let eq: Vec<&Meet> = meets.iter().filter(|m| m.op == Cmp::Eq).collect();
        let rest: Vec<Test> = meets
            .iter()
            .filter(|m| m.op != Cmp::Eq)
            .map(|m| Test::Two(*m))
            .chain(tests.iter().cloned())
            .collect();
        let map = self.by_key(side, &eq.iter().map(|m| m.a.column).collect::<Vec<_>>());
        let from: Vec<super::question::Col> = eq.iter().map(|m| m.b).collect();
        let every: Vec<u32> = (0..self.held[side].cells.len() as u32).collect();
        let mut out = Joined {
            width: j.width,
            rows: Vec::new(),
            weights: Vec::new(),
        };
        let mut row: Vec<u32> = vec![NONE; j.width];
        for i in 0..j.len() {
            row.copy_from_slice(j.row(i));
            let found: &[u32] = if eq.is_empty() {
                &every
            } else {
                match self.key(&row, &from).and_then(|k| map.get(&k)) {
                    Some(rows) => rows,
                    None => &[],
                }
            };
            let mut joined = false;
            for &r in found {
                row[side] = r;
                if rest.iter().all(|t| self.test(t, &row)) {
                    out.push(&row, j.weights[i] * self.held[side].weights[r as usize]);
                    joined = true;
                }
            }
            if outer && !joined {
                row[side] = NONE;
                out.push(&row, j.weights[i]);
            }
        }
        out
    }
}

impl Snail {
    /// A question on several tables: each read in turn, kept to the keys of the tables read before
    /// it that it meets, then met in memory. One round.
    pub(super) fn ask_join(&self, q: &Question) -> Result<(RecordBatch, Round)> {
        let at: Vec<(usize, usize)> = q.sides.iter().map(|s| (0, s.table)).collect();
        ask_join_across(&[self], &at, q)
    }
}

/// What a round says where a table it would hold in memory may not fit the room: the pipeline over
/// several relations reads the question, its rows lying in the room and past it written.
const PAST_THE_ROOM: &str = "a join's rows lie past the round's room";

thread_local! {
    /// The reads of a round that stopped so, for the round that takes it up.
    static STOPPED: std::cell::RefCell<Option<Round>> = const { std::cell::RefCell::new(None) };
}

/// What a round says where the rows it holds pass the room.
pub(super) fn stopped_for_room() -> crate::Error {
    crate::Error::new(PAST_THE_ROOM)
}

/// A round that stopped for its room, its reads so far kept for the round that takes it up.
pub(super) fn stop_with(round: Round) {
    if let Some(room) = crate::room::current() {
        room.stop();
    }
    STOPPED.with(|s| *s.borrow_mut() = Some(round));
}

/// Whether a round stopped for its room.
pub(super) fn past_the_room(e: &crate::Error) -> bool {
    e.0 == PAST_THE_ROOM
}

/// The reads of the round that stopped for its room.
pub(super) fn stopped_reads() -> Option<Round> {
    STOPPED.with(|s| s.borrow_mut().take())
}

/// A question on several tables, each read from the snail that holds it: `at` names, for each of
/// the question's tables, its snail among `snails` and its place among that snail's tables. One
/// round, each snail's part of it in the round's `snails` where there is more than one.
pub(super) fn ask_join_across(
    snails: &[&Snail],
    at: &[(usize, usize)],
    q: &Question,
) -> Result<(RecordBatch, Round)> {
    ask_join_within(snails, at, q, true, None)
}

/// A question on several tables whose rows are read as a place of the pipeline: its rows go to `each`
/// a block's at a time.
pub(super) fn read_join(
    snails: &[&Snail],
    at: &[(usize, usize)],
    q: &Question,
    each: &mut dyn FnMut(Vec<Vec<super::pipeline::Value>>) -> Result<()>,
) -> Result<Round> {
    let mut hand = |b: RecordBatch| each(super::pipeline::relation_of(&b)?.rows);
    let (last, round) = ask_join_within(snails, at, q, true, Some(&mut hand))?;
    hand(last)?;
    Ok(round)
}

/// The same, where `may_stop`, stopped by name before a table it would hold that its counts say may
/// not fit the round's room. Where `each` is given, the rows it makes go to it as they are made, a
/// block's at a time, and what it returns is the last of them.
pub(super) fn ask_join_within(
    snails: &[&Snail],
    at: &[(usize, usize)],
    q: &Question,
    may_stop: bool,
    each: Option<&mut dyn FnMut(RecordBatch) -> Result<()>>,
) -> Result<(RecordBatch, Round)> {
    let n = q.sides.len();
    let tables: Vec<&Table> = at
        .iter()
        .map(|&(s, t)| &snails[s].catalog.tables[t])
        .collect();
    // Each table's own conditions, as the snail that holds it numbers its tables.
    let sides: Vec<super::question::Side> = q
        .sides
        .iter()
        .zip(at)
        .map(|(side, &(_, t))| {
            let mut side = side.clone();
            side.table = t;
            side
        })
        .collect();
    let mut parts: Vec<Round> = vec![Round::default(); snails.len()];
    let order = read_order(q, &tables);
    let inputs = q.select.inputs();
    let (core, stream) = core_of(q, &order);
    let last = *order.last().unwrap();
    let (needed, answers) = needed_of(q);

    let mut t = Tables {
        tables: tables.clone(),
        columns: vec![Vec::new(); n],
        held: (0..n).map(|_| Held::default()).collect(),
    };
    let mut round = Round::default();
    let mut names = Vec::new();
    let ran: Vec<(u64, u64)> = snails
        .iter()
        .map(|s| (s.walked.get(), s.taken.get()))
        .collect();
    // A table that may stand NULL gives the answer NULLs, whatever its columns hold.
    let shown: Vec<Table> = (0..n)
        .map(|s| {
            let mut table = tables[s].clone();
            if q.nullable(s) {
                for c in &mut table.columns {
                    c.not_null = false;
                }
            }
            table
        })
        .collect();
    let shown: Vec<&Table> = shown.iter().collect();
    let mut answer = Answer::new(&q.select).typed(&tables);
    let mut sides_held = crate::room::holding(0);
    let mut hand_on = each.map(|each| {
        let (select, order, tables, shown) = (&q.select, &q.order, &tables, &shown);
        move |answer: &mut Answer| -> Result<()> {
            let done = std::mem::replace(answer, Answer::new(select).typed(tables));
            each(done.finish(shown, select, order)?)
        }
    });
    // Rows a join holds are counted, a round that may stop stopping before they pass the room.
    let room_over = || may_stop && crate::room::current().is_some_and(|r| r.over());
    for (i, &side) in order.iter().enumerate() {
        let (at_snail, _) = at[side];
        let snail = snails[at_snail];
        let table = tables[side];
        let streamed = stream && side == last;
        let Reading {
            by,
            led: kept_by,
            probe,
            columns,
            lead,
        } = reading(q, &order, i, table, streamed, &needed[side]);
        let keys = by.first().zip(kept_by.as_ref()).map(|((s, _), meets)| {
            let values = t.held[*s].cells.iter().filter_map(|cells| {
                meets
                    .iter()
                    .map(|m| {
                        level_value(
                            &cells[t.at(*s, m.b.column, false)],
                            t.ty(*s, m.b.column),
                            &table.columns[m.a.column],
                        )
                    })
                    .collect::<Option<Vec<KeyValue>>>()
            });
            KeySet::new(
                meets.iter().map(|m| m.a.column).collect(),
                values.collect::<Vec<_>>(),
            )
        });
        t.columns[side] = columns.clone();
        let mut keys = keys;
        if keys.is_none() {
            let alone = Want {
                table,
                side: &sides[side],
                columns: columns.iter().map(|c| c.0).collect(),
                countable: None,
                keys: None,
            };
            if let super::elements::Along::Strands(c) = snail.along_elements(q, &alone, None)? {
                let (found, strands) = snail.keys_of(table, &c)?;
                keys = found;
                for r in [&mut round, &mut parts[at_snail]] {
                    r.add_table(&strands, &strands.order);
                }
                names.push(strands.order);
            }
        }
        let want = Want {
            table,
            side: &sides[side],
            columns: columns.iter().map(|c| c.0).collect(),
            countable: None,
            keys: keys.as_ref(),
        };
        let mut mine = Round::default();
        let (before, held_before) = (snail.touched(), snail.pages_held());
        let part = snail.part(&want, None)?;
        snail.mark_read(want.side.table, &part.plan.walk)?;
        let width: u64 = columns
            .iter()
            .map(|c| super::strands::held_width(table.columns[c.0].ty))
            .sum();
        if may_stop && !streamed {
            let free = crate::room::current().map_or(u64::MAX, |r| r.free_bytes());
            if part.rows.1.saturating_mul(width) > free {
                if snails.len() > 1 {
                    round.snails = parts;
                }
                stop_with(round);
                return Err(crate::Error::new(PAST_THE_ROOM));
            }
        }
        mine.directory_pages = snail.touched() - before;
        mine.directory_pages_held = snail.pages_held() - held_before;
        mine.directory_pages_predicted = mine.directory_pages + part.plan.map_pages;
        snail.keep_held(&part.plan, &want)?;
        let mid = snail.reader.pages_read();
        let mut read = Ok(());
        if streamed {
            let joined = meet_core(q, &t, &core[..core.len() - 1]);
            sides_held.more(joined.rows.len() as u64 * 4);
            if room_over() {
                snail.reader.read_done();
                if snails.len() > 1 {
                    round.snails = parts;
                }
                stop_with(round);
                return Err(crate::Error::new(PAST_THE_ROOM));
            }
            let mut sink = Probe::new(q, &t, &joined, side, &probe, &inputs, &mut answer);
            sink.hand_on = hand_on
                .as_mut()
                .map(|h| h as &mut dyn FnMut(&mut Answer) -> Result<()>);
            read = snail.read_plan(&part.plan, &want, &columns, &mut sink);
        } else {
            let filters: Vec<Filter> = by
                .iter()
                .map(|(s, meets)| Filter {
                    at: meets
                        .iter()
                        .map(|m| {
                            columns
                                .iter()
                                .position(|c| *c == (m.a.column, false))
                                .unwrap()
                        })
                        .collect(),
                    types: meets.iter().map(|m| table.columns[m.a.column].ty).collect(),
                    keys: t.held[*s]
                        .cells
                        .iter()
                        .filter_map(|cells| {
                            meets
                                .iter()
                                .map(|m| {
                                    part_of(
                                        &cells[t.at(*s, m.b.column, false)],
                                        t.ty(*s, m.b.column),
                                    )
                                })
                                .collect::<Option<Vec<Part>>>()
                        })
                        .map(|k| (k, ()))
                        .collect(),
                })
                .collect();
            let mut held = Held::default();
            let mut sink = Gather {
                held: &mut held,
                index: (!answers[side]).then(WordMap::default),
                filters: &filters,
                lead: if filters.is_empty() { 0 } else { lead },
                dicts: Vec::new(),
                probe: Vec::new(),
            };
            snail.read_plan(&part.plan, &want, &columns, &mut sink)?;
            sides_held.more(held.cells.len() as u64 * width);
            t.held[side] = held;
        }
        snail.reader.read_done();
        let maps = part.plan.maps_read();
        mine.column_pages_read = snail.reader.pages_read() - mid - maps.file;
        mine.column_pages_held = part.plan.held_read();
        mine.directory_pages += maps.touched;
        mine.directory_pages_held += maps.held;
        let w = &part.plan.walk;
        let name = table.orders[w.order].name.clone();
        mine.consulted = part.consulted;
        mine.column_pages_top_low = w.top.0;
        mine.column_pages_top_high = w.top.1;
        mine.column_pages_low = w.low;
        mine.column_pages_high = w.high;
        mine.column_pages_predicted = part.plan.column_pages;
        mine.entries = w.statuses.0 + w.statuses.1 + w.statuses.2;
        mine.entries_none = w.statuses.0;
        mine.entries_may = w.statuses.1;
        mine.entries_must = w.statuses.2;
        mine.blocks_read = part.plan.blocks_read();
        // Stopped for its room partway, the read counts what it read and no more.
        let stopped = read.as_ref().err().is_some_and(past_the_room);
        if stopped {
            mine.column_pages_predicted = mine.column_pages_read;
            mine.directory_pages_predicted = mine.directory_pages;
        }
        for r in [&mut round, &mut parts[at_snail]] {
            r.add_table(&mine, &name);
        }
        names.push(name);
        if let Err(e) = read {
            if stopped {
                if snails.len() > 1 {
                    round.snails = parts;
                }
                stop_with(round);
            }
            return Err(e);
        }
    }
    if !stream {
        let mut joined = if q.full {
            let o = &q.outer[0];
            let start = Joined {
                width: n,
                rows: (0..t.held[0].cells.len() as u32)
                    .flat_map(|r| {
                        let mut row = vec![NONE; n];
                        row[0] = r;
                        row
                    })
                    .collect(),
                weights: t.held[0].weights.clone(),
            };
            full_join(&t, start, o.side, &o.meets, &o.tests)
        } else {
            meet_core(q, &t, &core)
        };
        for o in q.outer.iter().skip(q.full as usize) {
            joined = t.join(&joined, o.side, &o.meets, &o.tests, true);
        }
        sides_held.more(joined.rows.len() as u64 * 4);
        if room_over() {
            if snails.len() > 1 {
                round.snails = parts;
            }
            stop_with(round);
            return Err(crate::Error::new(PAST_THE_ROOM));
        }
        let mut cells = vec![Cell::Null; inputs.len()];
        for i in 0..joined.len() {
            let row = joined.row(i);
            if !q.after.iter().all(|a| t.test(a, row)) {
                continue;
            }
            for (cell, k) in cells.iter_mut().zip(&inputs) {
                let c = k.col();
                *cell = t
                    .cell(row, c.side, c.column, matches!(k, Key::Month(_)))
                    .clone();
            }
            answer.add_times(&cells, joined.weights[i]);
            if i % 1024 == 1023 {
                match &mut hand_on {
                    Some(h) => h(&mut answer)?,
                    None => {
                        if let Err(e) = answer.within_room() {
                            if snails.len() > 1 {
                                round.snails = parts;
                            }
                            stop_with(round);
                            return Err(e);
                        }
                    }
                }
            }
        }
    }
    drop(hand_on);
    let batch = answer.finish(&shown, &q.select, &q.order)?;
    round.order = names.join("+");
    round.rows = batch.num_rows() as u64;
    for (i, s) in snails.iter().enumerate() {
        let (walked, taken) = (s.walked.get() - ran[i].0, s.taken.get() - ran[i].1);
        round.rows_walked += walked;
        round.rows_taken += taken;
        parts[i].rows_walked = walked;
        parts[i].rows_taken = taken;
    }
    if snails.len() > 1 {
        round.snails = parts;
    }
    Ok((batch, round))
}

/// The tables `sides` met on what every answer row holds of them: from the first, each next one
/// that meets one already met.
fn meet_core(q: &Question, t: &Tables, sides: &[usize]) -> Joined {
    let n = q.sides.len();
    let first = sides[0];
    let mut joined = Joined {
        width: n,
        rows: Vec::new(),
        weights: Vec::new(),
    };
    let mut row = vec![NONE; n];
    for (r, &w) in t.held[first].weights.iter().enumerate() {
        row[first] = r as u32;
        joined.push(&row, w);
    }
    let mut done = vec![first];
    while done.len() < sides.len() {
        let next = *sides
            .iter()
            .find(|&&s| {
                !done.contains(&s)
                    && q.meets
                        .iter()
                        .any(|m| m.touches(s) && done.iter().any(|&d| m.touches(d)))
            })
            .expect("the tables no outer join may leave NULL meet each other");
        let meets: Vec<Meet> = q
            .meets
            .iter()
            .filter(|m| m.touches(next) && done.iter().any(|&d| m.touches(d)))
            .map(|m| m.seen_from(next))
            .collect();
        joined = t.join(&joined, next, &meets, &[], false);
        done.push(next);
    }
    joined
}

/// The rows of the first table joined FULL with table `side`'s: each pair that meets and passes the
/// tests, then each row of either that joined none, the other NULL.
fn full_join(t: &Tables, j: Joined, side: usize, meets: &[Meet], tests: &[Test]) -> Joined {
    let map = t.by_key(side, &meets.iter().map(|m| m.a.column).collect::<Vec<_>>());
    let from: Vec<super::question::Col> = meets.iter().map(|m| m.b).collect();
    let mut matched = vec![false; t.held[side].cells.len()];
    let mut out = Joined {
        width: j.width,
        rows: Vec::new(),
        weights: Vec::new(),
    };
    let mut row = vec![NONE; j.width];
    for i in 0..j.len() {
        row.copy_from_slice(j.row(i));
        let mut joined = false;
        if let Some(rows) = t.key(&row, &from).and_then(|k| map.get(&k)) {
            for &r in rows {
                row[side] = r;
                if tests.iter().all(|x| t.test(x, &row)) {
                    out.push(&row, j.weights[i] * t.held[side].weights[r as usize]);
                    matched[r as usize] = true;
                    joined = true;
                }
            }
        }
        if !joined {
            row[side] = NONE;
            out.push(&row, j.weights[i]);
        }
    }
    let mut row = vec![NONE; j.width];
    for (r, m) in matched.iter().enumerate() {
        if !m {
            row[side] = r as u32;
            out.push(&row, t.held[side].weights[r]);
        }
    }
    out
}

/// The last table read, met as its rows arrive with the rows the others met: each of its rows with
/// the rows met that hold its key, or, where those give the answer nothing and no inequality reads
/// them, with how many rows of the answer they stand for.
struct Probe<'a> {
    t: &'a Tables<'a>,
    side: usize,
    answer: &'a mut Answer,
    inputs: &'a [Key],
    joined: &'a Joined,
    rows: WordMap<Vec<Part>, Vec<u32>>,
    counts: WordMap<Vec<Part>, u64>,
    alone: bool,
    /// Its comparisons with the tables met other than on its keys, each written from its column.
    tests: Vec<Meet>,
    /// Where its key's columns lie among those its scan returns, and their types.
    at: Vec<usize>,
    types: Vec<ColumnType>,
    lead: usize,
    dicts: Vec<Dict>,
    probe: Vec<Part>,
    cells: Vec<Cell>,
    /// What hands the rows made so far on, at each block's end.
    hand_on: Option<&'a mut dyn FnMut(&mut Answer) -> Result<()>>,
}

impl<'a> Probe<'a> {
    fn new(
        q: &Question,
        t: &'a Tables<'a>,
        joined: &'a Joined,
        side: usize,
        meets: &[Meet],
        inputs: &'a [Key],
        answer: &'a mut Answer,
    ) -> Probe<'a> {
        let others: Vec<super::question::Col> = meets.iter().map(|m| m.b).collect();
        let tests: Vec<Meet> = q
            .meets
            .iter()
            .filter(|m| m.touches(side) && m.op != Cmp::Eq)
            .map(|m| m.seen_from(side))
            .collect();
        let alone = tests.is_empty() && inputs.iter().all(|k| k.col().side == side);
        let mut rows: WordMap<Vec<Part>, Vec<u32>> = WordMap::default();
        let mut counts: WordMap<Vec<Part>, u64> = WordMap::default();
        for i in 0..joined.len() {
            if let Some(k) = t.key(joined.row(i), &others) {
                if alone {
                    *counts.entry(k).or_default() += joined.weights[i];
                } else {
                    rows.entry(k).or_default().push(i as u32);
                }
            }
        }
        let at: Vec<usize> = meets
            .iter()
            .map(|m| t.at(side, m.a.column, false))
            .collect();
        Probe {
            t,
            side,
            answer,
            inputs,
            joined,
            rows,
            counts,
            alone,
            tests,
            lead: at.iter().map(|a| a + 1).max().unwrap_or(0),
            types: meets.iter().map(|m| t.ty(side, m.a.column)).collect(),
            at,
            dicts: Vec::new(),
            probe: Vec::new(),
            cells: vec![Cell::Null; inputs.len()],
            hand_on: None,
        }
    }

    fn text(&self, at: usize, c: &Cell) -> Cell {
        as_text(c, self.dicts.get(at).unwrap_or(&None))
    }
}

impl Sink for Probe<'_> {
    fn block_end(&mut self) -> Result<()> {
        match &mut self.hand_on {
            Some(h) => h(self.answer),
            None => self.answer.within_room(),
        }
    }

    fn dictionaries(&mut self, dicts: &[Dict]) {
        self.dicts = dicts.to_vec();
        let mine: Vec<Dict> = self
            .inputs
            .iter()
            .map(|k| {
                let c = k.col();
                if c.side == self.side {
                    dicts[self.t.at(c.side, c.column, matches!(k, Key::Month(_)))].clone()
                } else {
                    None
                }
            })
            .collect();
        self.answer.dictionaries(&mine);
    }

    fn lead(&self) -> usize {
        self.lead
    }

    fn admits(&mut self, lead: &[Cell]) -> bool {
        self.probe.clear();
        for i in 0..self.at.len() {
            match part_of(&self.text(self.at[i], &lead[self.at[i]]), self.types[i]) {
                Some(p) => self.probe.push(p),
                None => return false,
            }
        }
        if self.alone {
            self.counts.contains_key(self.probe.as_slice())
        } else {
            self.rows.contains_key(self.probe.as_slice())
        }
    }

    fn row(&mut self, row: &[Cell]) {
        let side = self.side;
        let t = self.t;
        let mine = |k: &Key| t.at(side, k.col().column, matches!(k, Key::Month(_)));
        if self.alone {
            let Some(&n) = self.counts.get(self.probe.as_slice()) else {
                return;
            };
            for (cell, k) in self.cells.iter_mut().zip(self.inputs) {
                *cell = row[mine(k)].clone();
            }
            self.answer.add_times(&self.cells, n);
            return;
        }
        let Some(found) = self.rows.get(self.probe.as_slice()) else {
            return;
        };
        let compared: Vec<Cell> = self
            .tests
            .iter()
            .map(|m| {
                self.text(
                    t.at(side, m.a.column, false),
                    &row[t.at(side, m.a.column, false)],
                )
            })
            .collect();
        for &i in found {
            let met = self.joined.row(i as usize);
            let passes = self.tests.iter().zip(&compared).all(|(m, a)| {
                compares(
                    m.op,
                    a,
                    t.ty(side, m.a.column),
                    t.cell(met, m.b.side, m.b.column, false),
                    t.ty(m.b.side, m.b.column),
                )
            });
            if !passes {
                continue;
            }
            for (cell, k) in self.cells.iter_mut().zip(self.inputs) {
                let c = k.col();
                *cell = if c.side == side {
                    row[mine(k)].clone()
                } else {
                    t.cell(met, c.side, c.column, matches!(k, Key::Month(_)))
                        .clone()
                };
            }
            self.answer
                .add_times(&self.cells, self.joined.weights[i as usize]);
        }
    }
}

/// Whether the tables `sides` meet each other on what every answer row holds of them.
fn connected(q: &Question, sides: &[usize]) -> bool {
    let Some(&first) = sides.first() else {
        return false;
    };
    let mut done = vec![first];
    while let Some(&s) = sides.iter().find(|&&s| {
        !done.contains(&s)
            && q.meets
                .iter()
                .any(|m| m.touches(s) && done.iter().any(|&d| m.touches(d)))
    }) {
        done.push(s);
    }
    done.len() == sides.len()
}

/// The equalities by which table `side` may be kept to the keys table `by` holds, each written from
/// `side`'s column: those every answer row holds, and those of `side`'s own outer join's ON, which
/// joins its rows only to rows of `by` that meet them. A FULL JOIN keeps both its tables.
fn keeps(q: &Question, side: usize, by: usize) -> Vec<Meet> {
    let mut out: Vec<Meet> = q
        .meets
        .iter()
        .filter(|m| m.op == Cmp::Eq && m.touches(side) && m.touches(by))
        .map(|m| m.seen_from(side))
        .collect();
    if !q.full {
        for o in q.outer.iter().filter(|o| o.side == side) {
            out.extend(o.meets.iter().filter(|m| m.b.side == by).copied());
        }
    }
    out
}

/// The tables no outer join may leave NULL, in the order they are read, each joined to one joined
/// before it; and whether the last read is met as it is read, where it alone joins it to them.
pub(super) fn core_of(q: &Question, order: &[usize]) -> (Vec<usize>, bool) {
    let core: Vec<usize> = order
        .iter()
        .copied()
        .filter(|&s| !q.nullable(s) && !(q.full && s < 2))
        .collect();
    let last = *order.last().unwrap();
    let stream = q.outer.is_empty()
        && !q.full
        && q.after.is_empty()
        && q.meets.iter().any(|m| m.touches(last) && m.op == Cmp::Eq)
        && connected(q, &core[..core.len() - 1]);
    (core, stream)
}

/// Every column the join reads of each table, then the answer's; and whether the answer reads the
/// table.
pub(super) fn needed_of(q: &Question) -> (Vec<Vec<(usize, bool)>>, Vec<bool>) {
    let n = q.sides.len();
    let mut needed: Vec<Vec<(usize, bool)>> = vec![Vec::new(); n];
    let mut add = |side: usize, c: (usize, bool)| {
        if !needed[side].contains(&c) {
            needed[side].push(c);
        }
    };
    let tests = q
        .after
        .iter()
        .chain(q.outer.iter().flat_map(|o| o.tests.iter()));
    for t in tests {
        match t {
            Test::One(s, p) => {
                if let Some(c) = p.column() {
                    add(*s, c);
                }
            }
            Test::Two(m) => {
                add(m.a.side, (m.a.column, false));
                add(m.b.side, (m.b.column, false));
            }
        }
    }
    for m in q
        .meets
        .iter()
        .chain(q.outer.iter().flat_map(|o| o.meets.iter()))
    {
        add(m.a.side, (m.a.column, false));
        add(m.b.side, (m.b.column, false));
    }
    let mut answers = vec![false; n];
    for k in &q.select.inputs() {
        answers[k.col().side] = true;
        add(k.col().side, (k.col().column, matches!(k, Key::Month(_))));
    }
    (needed, answers)
}

/// A table's part of a join's round, known before any row is read: the tables read before it that
/// it may be kept to, the one read last first, each with the equalities it is kept by; those of the
/// one read last in the order of an order's leading levels, which its walk is kept by; the
/// equalities its rows meet as they are read, where it is read last and met so; and the columns its
/// scan returns, the keys it is kept to first, `lead` of them.
pub(super) struct Reading {
    pub(super) by: Vec<(usize, Vec<Meet>)>,
    pub(super) led: Option<Vec<Meet>>,
    pub(super) probe: Vec<Meet>,
    pub(super) columns: Vec<(usize, bool)>,
    pub(super) lead: usize,
}

/// The `i`-th table of `order` as a join's round reads it.
pub(super) fn reading(
    q: &Question,
    order: &[usize],
    i: usize,
    table: &Table,
    streamed: bool,
    needed: &[(usize, bool)],
) -> Reading {
    let side = order[i];
    let by: Vec<(usize, Vec<Meet>)> = order[..i]
        .iter()
        .rev()
        .map(|&s| (s, keeps(q, side, s)))
        .filter(|(_, m)| !m.is_empty())
        .collect();
    let led = by.first().map(|(_, meets)| led(table, meets));
    // The walk's keys, then every other key it is kept to, then the rest.
    let mut columns: Vec<(usize, bool)> = Vec::new();
    for m in led.iter().flatten() {
        if !columns.contains(&(m.a.column, false)) {
            columns.push((m.a.column, false));
        }
    }
    let probe: Vec<Meet> = if streamed {
        q.meets
            .iter()
            .filter(|m| m.touches(side) && m.op == Cmp::Eq)
            .map(|m| m.seen_from(side))
            .collect()
    } else {
        Vec::new()
    };
    for m in by.iter().flat_map(|(_, m)| m).chain(&probe) {
        if !columns.contains(&(m.a.column, false)) {
            columns.push((m.a.column, false));
        }
    }
    let lead = columns.len();
    for c in needed {
        if !columns.contains(c) {
            columns.push(*c);
        }
    }
    Reading {
        by,
        led,
        probe,
        columns,
        lead,
    }
}

/// The equalities a table is kept by, in the order of the leading levels of the first of its orders
/// they lead, where one does.
fn led(table: &Table, meets: &[Meet]) -> Vec<Meet> {
    for o in &table.orders {
        if o.levels.len() < meets.len() {
            continue;
        }
        let lead: Option<Vec<Meet>> = o.levels[..meets.len()]
            .iter()
            .map(|l| {
                meets
                    .iter()
                    .find(|m| *l == Level::Column(m.a.column))
                    .copied()
            })
            .collect();
        if let Some(lead) = lead {
            let mut seen: Vec<usize> = lead.iter().map(|m| m.a.column).collect();
            seen.dedup();
            if seen.len() == meets.len() {
                return lead;
            }
        }
    }
    meets.to_vec()
}

/// Whether a table's own conditions fix places on one of its orders: a condition on one of their
/// levels, or one no row passes.
fn fixed(table: &Table, side: &super::question::Side) -> bool {
    let on_level = |l: Level| table.orders.iter().any(|o| o.levels.contains(&l));
    side.empty
        || side.conds.iter().any(|c| match c.shape {
            crate::catalog::Shape::MonthUtc(x) => on_level(Level::MonthUtc(x)),
            crate::catalog::Shape::Scaled(x, k) => on_level(Level::Scaled(x, k)),
            crate::catalog::Shape::Column(x) | crate::catalog::Shape::Clock(x) => {
                on_level(Level::Column(x))
            }
        })
        || side.texts.iter().any(|t| on_level(Level::Column(t.column)))
        || side.nulls.iter().any(|&(c, _)| on_level(Level::Column(c)))
}

/// The order the round reads the tables in: first each table no outer join may leave NULL whose own
/// conditions fix places on one of its orders, as written, or, where none does, the first such
/// table as written; then, again and again, the first table as written that may be kept to the keys
/// of the table read last that keeps one, and where none may, the next table as written that no
/// outer join may leave NULL, then any. A FULL JOIN's two tables are read first, by their own
/// conditions.
pub(super) fn read_order(q: &Question, tables: &[&Table]) -> Vec<usize> {
    let n = q.sides.len();
    let mut order: Vec<usize> = if q.full {
        vec![0, 1]
    } else {
        let inner: Vec<usize> = (0..n).filter(|&s| !q.nullable(s)).collect();
        let starts: Vec<usize> = inner
            .iter()
            .copied()
            .filter(|&s| fixed(tables[s], &q.sides[s]))
            .collect();
        if starts.is_empty() {
            vec![inner[0]]
        } else {
            starts
        }
    };
    while order.len() < n {
        let next = order
            .iter()
            .rev()
            .find_map(|&by| (0..n).find(|&s| !order.contains(&s) && !keeps(q, s, by).is_empty()))
            .or_else(|| (0..n).find(|&s| !order.contains(&s) && !q.nullable(s)))
            .or_else(|| (0..n).find(|&s| !order.contains(&s)))
            .unwrap();
        order.push(next);
    }
    order
}
