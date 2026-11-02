//! A question in PostgreSQL's SQL, read off a snail: for each table it reads, the orders that serve it,
//! each one's pages predicted from the directory before any column page is read, then the read.
//!
//! Several tables joined are read one after another, from the question's scopes, each kept to the
//! keys the tables read before it hold (`join`).

mod across;
mod aggregates;
mod answer;
mod arrays;
mod beside;
mod calls;
mod change;
mod clock;
mod collate;
pub(crate) mod elements;
pub(crate) mod fill;
pub(crate) mod foreign;
pub mod handed;
mod hash;
mod join;
mod json;
mod level;
mod locking;
mod out;
pub(crate) mod partial;
mod pipeline;
mod point;
mod populate;
mod prefix;
mod question;
mod ranges;
mod relations;
mod rest;
pub(crate) mod serial;
mod sets;
mod spill;
mod spelled;
mod strands;
mod summed;
pub mod statement;
mod subqueries;
mod together;
mod tsearch;
mod typed;
mod walk;

pub use across::{Snails, StopAcross};
pub use change::Done;
pub use level::{held_of_key, LevelCounts, PlaceRows};
pub use out::{TableOut, DDL_KEY, MONETARY_KEY, RELEASE_KEY};
pub use pipeline::{numeric_text, Value};
pub use point::Point;
pub use together::{
    answer_together, at_one_moment, begin_together, price_stopped, price_together, Answering,
    Priced, Stopped, TablePrice,
};

/// An answer's rows as values, as a pipeline reads them.
pub fn pipeline_rows(batch: &RecordBatch) -> Result<Vec<Vec<Value>>> {
    Ok(pipeline::relation_of(batch)?.rows)
}

/// The names of the columns a view's question returns, read over the catalog as PostgreSQL's
/// parser reads it as the view is made: no row read.
pub(crate) fn view_columns(catalog: &Catalog, q: &sqlparser::ast::Query) -> Result<Vec<String>> {
    relations::checked(catalog, q)
}

impl relations::Reads for Snail {
    fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    fn read_join(
        &self,
        q: &Question,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        let at: Vec<(usize, usize)> = q.sides.iter().map(|s| (0, s.table)).collect();
        join::read_join(&[self], &at, q, each)
    }

    fn read_each(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
        most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        self.read_blocks(q, order, kept, most, each)
    }

    fn strand_wins(&self, q: &Question, strand: &str) -> Result<bool> {
        let table = &self.catalog.tables[q.sides[0].table];
        let o = table.orders.iter().position(|o| o.name == strand);
        match o {
            Some(o) => Snail::strand_wins(self, q, table, o, None),
            None => Ok(false),
        }
    }

    fn snails(&self) -> usize {
        1
    }
}
pub use question::{instant, truth};
pub(crate) use typed::element_of as element_type;

use crate::catalog::{Catalog, Table};
use crate::cut::PAGE;
use crate::directory::{decode_page, maps_held, read_map, PageMap};
use crate::file::{Extent, Reader, Root, Writer};
use crate::places::{shown, Sublevels};
use crate::{Error, Result};
use answer::{Answer, Cell, Dict};
use arrow::array::RecordBatch;
use elements::Along;
use question::{Key, Question};
use std::path::Path;
use walk::{condition_columns, next_down, ruled_out, KeySet, Plan, Sink, Want};

/// What an open snail holds between rounds unless told otherwise: PostgreSQL's default
/// `shared_buffers`.
pub const HELD_BYTES: usize = 128 << 20;

/// A catalog as a snail of `format` holds its tables: their columns as the format's leaves, and
/// their orders as it names them: 0.1.x, formats 7 and 8, named a primary key's order for its
/// table, whatever its constraint was named.
pub(crate) fn old_names(catalog: &mut Catalog, format: u32) {
    for t in &mut catalog.tables {
        t.format = format;
    }
    if format >= 9 {
        return;
    }
    for t in &mut catalog.tables {
        let bare = t.name.rsplit('.').next().unwrap_or(&t.name).to_string();
        for o in &mut t.orders {
            if o.constraint.as_ref().is_some_and(|k| k.primary) {
                o.name = format!("{bare}_pkey");
            }
        }
    }
}

/// Calls `f` on each table a query's FROMs name, in its steps and in its body, each arm of a set
/// operation among them, and in its derived tables and subqueries.
fn each_table(q: &mut sqlparser::ast::Query, f: &mut dyn FnMut(&mut sqlparser::ast::TableFactor)) {
    if let Some(with) = q.with.as_mut() {
        for cte in &mut with.cte_tables {
            each_table(&mut cte.query, f);
        }
    }
    each_table_of(q.body.as_mut(), f);
    if let Some(sqlparser::ast::OrderBy {
        kind: sqlparser::ast::OrderByKind::Expressions(sorts),
        ..
    }) = q.order_by.as_mut()
    {
        for s in sorts {
            subqueries::each_query_mut(&mut s.expr, &mut |q| each_table(q, f));
        }
    }
}

fn each_table_of(
    body: &mut sqlparser::ast::SetExpr,
    f: &mut dyn FnMut(&mut sqlparser::ast::TableFactor),
) {
    use sqlparser::ast::{SelectItem, SetExpr, TableFactor};
    fn factor(t: &mut TableFactor, f: &mut dyn FnMut(&mut TableFactor)) {
        f(t);
        match t {
            TableFactor::Derived { subquery, .. } => each_table(subquery, f),
            TableFactor::NestedJoin {
                table_with_joins, ..
            } => {
                factor(&mut table_with_joins.relation, f);
                for j in &mut table_with_joins.joins {
                    factor(&mut j.relation, f);
                }
            }
            _ => {}
        }
    }
    match body {
        SetExpr::Select(s) => {
            for item in &mut s.from {
                factor(&mut item.relation, f);
                for j in &mut item.joins {
                    factor(&mut j.relation, f);
                }
            }
            let mut exprs: Vec<&mut sqlparser::ast::Expr> = Vec::new();
            for p in &mut s.projection {
                if let SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } = p {
                    exprs.push(e);
                }
            }
            exprs.extend(s.selection.as_mut());
            exprs.extend(s.having.as_mut());
            if let sqlparser::ast::GroupByExpr::Expressions(by, _) = &mut s.group_by {
                exprs.extend(by.iter_mut());
            }
            for e in exprs {
                subqueries::each_query_mut(e, &mut |q| each_table(q, f));
            }
        }
        SetExpr::SetOperation { left, right, .. } => {
            each_table_of(left, f);
            each_table_of(right, f);
        }
        SetExpr::Query(q) => each_table(q, f),
        _ => {}
    }
}

/// A question that reads a table's virtual columns, read through a first step that computes them
/// from the columns the table stores; none where it reads no such column.
fn computed(catalog: &Catalog, q: &sqlparser::ast::Query) -> Result<Option<sqlparser::ast::Query>> {
    use sqlparser::ast::{Ident, ObjectName, ObjectNamePart, TableAlias, TableFactor};
    let mut read: Vec<usize> = Vec::new();
    let mut q = q.clone();
    each_table(&mut q, &mut |t| {
        if let TableFactor::Table { name, .. } = t {
            let n = crate::catalog::object_name(name);
            if let Some(i) = catalog.tables.iter().position(|x| x.name == n) {
                if !read.contains(&i) {
                    read.push(i);
                }
            }
        }
    });
    let text = q.to_string();
    let words: Vec<String> =
        sqlparser::tokenizer::Tokenizer::new(&sqlparser::dialect::PostgreSqlDialect {}, &text)
            .tokenize()?
            .into_iter()
            .filter_map(|t| match t {
                sqlparser::tokenizer::Token::Word(w) => Some(if w.quote_style.is_some() {
                    w.value
                } else {
                    w.value.to_lowercase()
                }),
                _ => None,
            })
            .collect();
    let wildcard = text.contains(".*") || {
        let mut any = false;
        if let sqlparser::ast::SetExpr::Select(s) = q.body.as_ref() {
            any = s.projection.iter().any(|p| {
                matches!(
                    p,
                    sqlparser::ast::SelectItem::Wildcard(_)
                        | sqlparser::ast::SelectItem::QualifiedWildcard(..)
                )
            });
        }
        any
    };
    let mut steps = Vec::new();
    for &i in &read {
        let t = &catalog.tables[i];
        let reads = t
            .columns
            .iter()
            .any(|c| c.is_virtual() && (wildcard || words.contains(&c.name)));
        if !reads {
            continue;
        }
        let quote = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let items: Vec<String> = t
            .columns
            .iter()
            .map(|c| match &c.generated {
                Some(g) if !g.stored => format!("({}) AS {}", g.expr, quote(&c.name)),
                _ => quote(&c.name),
            })
            .collect();
        let step = format!("snail_computed_{i}");
        steps.push((
            i,
            step.clone(),
            format!("{step} AS (SELECT {} FROM {})", items.join(", "), t.name),
        ));
    }
    if steps.is_empty() {
        return Ok(None);
    }
    each_table(&mut q, &mut |f| {
        if let TableFactor::Table { name, alias, .. } = f {
            let n = crate::catalog::object_name(name);
            if let Some((i, step, _)) = steps.iter().find(|(i, _, _)| catalog.tables[*i].name == n)
            {
                let bare = catalog.tables[*i]
                    .name
                    .rsplit('.')
                    .next()
                    .unwrap()
                    .to_string();
                *name = ObjectName(vec![ObjectNamePart::Identifier(Ident::new(step.clone()))]);
                if alias.is_none() {
                    *alias = Some(TableAlias {
                        explicit: true,
                        name: Ident::new(bare),
                        columns: vec![],
                        at: None,
                    });
                }
            }
        }
    });
    let with: Vec<String> = steps.into_iter().map(|(_, _, s)| s).collect();
    let rest = match &q.with {
        Some(w) => {
            let mut q2 = q.clone();
            q2.with = None;
            format!(
                "WITH {}, {} {q2}",
                with.join(", "),
                w.cte_tables
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
        None => format!("WITH {} {q}", with.join(", ")),
    };
    match sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &rest)?
        .as_slice()
    {
        [sqlparser::ast::Statement::Query(q)] => Ok(Some((**q).clone())),
        _ => Err(Error::new("a question with virtual columns is not read")),
    }
}

/// `n` of the ordinals below `total`, each set of `n` as likely as any other, ascending; every one
/// where `n` is no fewer. `random` draws a number from 0 up to 1.
fn drawn(total: u64, n: u64, random: &mut dyn FnMut() -> f64) -> Vec<u64> {
    if n >= total {
        return (0..total).collect();
    }
    let mut picked = std::collections::BTreeSet::new();
    for j in total - n..total {
        let t = ((random() * (j + 1) as f64) as u64).min(j);
        if !picked.insert(t) {
            picked.insert(j);
        }
    }
    picked.into_iter().collect()
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
    /// Whether a statement of the transaction under way failed: it is rolled back at its end.
    aborted: bool,
    /// The isolation levels of its transactions, as they are named.
    pub(crate) levels: statement::Levels,
    /// Where the snail is open to write, its writer's slot among the file's writers.
    slot: u64,
    /// The blocks a round read a page or more of waste in, which it prunes as it ends.
    wasted: std::cell::RefCell<Vec<Wasted>>,
    /// The blocks the statement or the transaction under way has written, by table and order, each
    /// against the block it began from.
    rebased: std::collections::BTreeMap<(usize, usize), Vec<change::Rebased>>,
    /// The blocks the statement or the transaction under way has locked, by their lock bytes; and
    /// whether the statement found one written since the root it read.
    locks: std::collections::BTreeSet<u64>,
    stale: bool,
    /// The bytes of the rows the transaction under way has locked, each with whether it is held
    /// shared, and of the tables whose rows it locked or changed; and the tables whose rows it
    /// changed without naming them.
    row_locks: std::collections::BTreeMap<u64, bool>,
    unnamed: std::collections::BTreeSet<usize>,
    /// Whether the transaction under way, at REPEATABLE READ, holds the generation its first read
    /// took.
    held: bool,
    /// The pages its commits have written into pages freed before.
    reused: u64,
    /// The catalog the statement or the transaction under way has declared.
    declared: Option<change::Declared>,
    /// The rows the scans have walked one by one, and taken in runs at once.
    walked: std::cell::Cell<u64>,
    taken: std::cell::Cell<u64>,
    /// The statement's clock, in microseconds since 1970 UTC: where the transaction began, or
    /// the statement where it stands alone; and where the statement began.
    clock: (i64, i64),
    /// The values each sequence has handed this writer and it has not used, as PostgreSQL's
    /// sequences cache them a session.
    handed: std::collections::HashMap<String, std::collections::VecDeque<i64>>,
    /// The sequences' states a commit at the root's turn writes.
    advancing: Option<Vec<crate::file::SequenceState>>,
    /// Where the snail is open to write, what its writer shares among the snails it writes, and
    /// the snail's place among them.
    group: Option<(std::rc::Rc<across::Group>, usize)>,
    /// The transaction across snails the next commit decides: the root it writes into each other
    /// snail, and their bytes, which the commit carries.
    deciding: Option<(Vec<crate::file::Written>, Vec<u8>)>,
    /// What the statement under way changed that its foreign keys check as it ends; what the
    /// transaction changed that they check at the commit; and the keys SET CONSTRAINTS defers, or
    /// makes immediate, until it ends.
    touched: foreign::Touched,
    pending: foreign::Touched,
    modes: std::collections::BTreeMap<(usize, usize), bool>,
    /// The source whose applier the writer presents itself as, where it does.
    applier: Option<String>,
    /// The replica mark the statement or the transaction under way sets, or clears.
    marking: Option<Option<String>>,
    /// The functions its caller has handed it, which its questions call.
    functions: std::rc::Rc<Vec<std::rc::Rc<handed::Function>>>,
    /// The serializable transaction under way, and what the next commit writes for one rolled back;
    /// or the host's monitor that is told of its rounds, the file keeping no marks.
    serial: std::cell::RefCell<Option<serial::Serial>>,
    ending: Option<serial::Record>,
    monitor: Option<std::rc::Rc<dyn crate::carry::Monitor>>,
    /// Whether the round under way reads the newest root only to compare a statement's rows with it,
    /// the statement to be refused or to go on as it found them: it reads nothing of its own.
    comparing: std::cell::Cell<bool>,
    /// What a transaction of one snapshot has changed so far, by table, which its commit writes into
    /// the newest root; its snapshot as it took it; whether that commit is under way; and for each block of the version it reads
    /// whose first key its rows lowered, the lock byte of the first key its snapshot gives it.
    own: std::collections::BTreeMap<String, beside::OwnRows>,
    own_from: Option<Root>,
    committing: bool,
    first_bytes: std::collections::BTreeMap<u64, u64>,
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
#[non_exhaustive]
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
#[non_exhaustive]
pub struct Round {
    /// The order read, or for a join the order read of each table, the first table read first.
    pub order: String,
    /// The orders that hold every column the question reads; none are listed for a join.
    pub served: Vec<String>,
    pub consulted: Vec<Consulted>,
    /// The pages read from the file as the round began, after the first round of an open snail: the
    /// root's two copies, and the catalog on a new generation. The first round's are the open's.
    pub pages_at_start: u64,
    /// The directory pages the round touched, and of them, those found held; and those it was to touch,
    /// known before it read a column page: its blocks' maps it reads as it reaches them among them.
    pub directory_pages: u64,
    pub directory_pages_held: u64,
    pub directory_pages_predicted: u64,
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
    /// For a round across several snails, each snail's part of it, in the order they were opened:
    /// the root pages it read as the round began, and what it read of each of its tables.
    pub snails: Vec<Round>,
    /// The most bytes the round held at once: the rows its places held, at their held width, and
    /// the pages it kept for its later reads.
    pub bytes_held: u64,
    /// The pages the round wrote to its spill, the rows past its room, and those it read back.
    pub spill_pages_written: u64,
    pub spill_pages_read: u64,
    /// For a question read as a pipeline over several relations, each of its reads in the order it
    /// made them, each priced before its pages were read: the round's pages are their sum.
    pub reads: Vec<Round>,
}

impl Round {
    /// Adds one table's reading, along its order `name`, to a round of several.
    pub(crate) fn add_table(&mut self, table: &Round, name: &str) {
        self.order = match self.order.is_empty() {
            true => name.to_string(),
            false => format!("{}+{name}", self.order),
        };
        self.consulted.extend(table.consulted.iter().cloned());
        self.directory_pages += table.directory_pages;
        self.directory_pages_held += table.directory_pages_held;
        self.directory_pages_predicted += table.directory_pages_predicted;
        self.column_pages_top_low += table.column_pages_top_low;
        self.column_pages_top_high += table.column_pages_top_high;
        self.column_pages_low += table.column_pages_low;
        self.column_pages_high += table.column_pages_high;
        self.column_pages_predicted += table.column_pages_predicted;
        self.column_pages_held += table.column_pages_held;
        self.column_pages_read += table.column_pages_read;
        self.entries += table.entries;
        self.entries_none += table.entries_none;
        self.entries_may += table.entries_may;
        self.entries_must += table.entries_must;
        self.blocks_read += table.blocks_read;
    }
}

/// One table's part of a round: the plan read, the orders that served it, and what each said; and
/// the rows the question holds, as the counts of every order walked bound them, and the table's rows.
struct Part {
    plan: Plan,
    served: Vec<String>,
    consulted: Vec<Consulted>,
    rows: (u64, u64),
    table_rows: u64,
}

/// A question on one table priced from the directory alone: the order a round would read along, the
/// pages it would read, and the rows the counts bound, without a column page read.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Price {
    pub order: String,
    /// The orders that hold every column the question reads, and what each said.
    pub served: Vec<String>,
    pub consulted: Vec<Consulted>,
    /// The rows the table holds.
    pub table_rows: u64,
    /// The fewest and the most rows the question can hold: every order walked bounds them alike.
    pub rows_low: u64,
    pub rows_high: u64,
    /// The rows the question holds, estimated from the page maps of the blocks the order reads, between
    /// the two: each page's rows taken as lying evenly across its keys, and an entry's across its least
    /// to greatest.
    pub rows_estimate: u64,
    /// The columns whose conditions the order read holds no key for: text off its levels, or one on a
    /// text level that keeps more than a run, as `<>` does. The estimate takes the blocks they open
    /// whole.
    pub keyless: Vec<String>,
    /// The directory pages the pricing read, page maps among them; those a round along the order goes
    /// on to read of the maps of the blocks it reaches; and the column pages it reads, exactly.
    pub directory_pages: u64,
    pub map_pages: u64,
    pub column_pages: u64,
    pub blocks: u64,
    /// The bytes a round holds of a row it reads: 0 where it reads no column page.
    pub row_bytes: u64,
}

/// The bytes a round holds of a row of `table` a plan reads, of `columns`: each at its held width, or
/// the bytes a row takes of it in the pages read where those are more. None where it reads no page.
fn row_bytes(plan: &Plan, table: &Table, columns: impl Iterator<Item = usize>) -> u64 {
    if plan.column_pages == 0 {
        return 0;
    }
    let order = &table.orders[plan.walk.order];
    columns
        .map(|c| {
            let written = order
                .slots(table, c)
                .map_or(0, |slots| plan.bytes_a_row(slots));
            strands::held_width(table.columns[c].ty).max(written)
        })
        .sum()
}

/// A round handed out a block at a time: `Snail::scan` begins it on the root the snail last read,
/// `Snail::next_block` hands out each block's rows, and `round` says what it has read so far. A
/// question that groups, counts or orders its rows comes out in one batch at the end.
#[non_exhaustive]
pub struct Scan {
    q: Question,
    /// The keys its element strands found, which it is kept to, and their round.
    kept: Option<KeySet>,
    strands: Round,
    plan: Plan,
    columns: Vec<(usize, bool)>,
    next: usize,
    whole: bool,
    done: bool,
    generation: u64,
    counted: (u64, [u64; 12]),
    round: Round,
}

impl Scan {
    /// What the round has read so far, and the rows it has handed out.
    pub fn round(&self) -> Round {
        if self.strands.order.is_empty() {
            return self.round.clone();
        }
        let mut r = self.strands.clone();
        r.add_table(&self.round, &self.round.order);
        r.served = self.round.served.clone();
        r.rows = self.round.rows;
        r.rows_walked += self.round.rows_walked;
        r.rows_taken += self.round.rows_taken;
        r.reads.push(self.round.clone());
        r
    }
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
            aborted: false,
            levels: Default::default(),
            slot: 0,
            wasted: Default::default(),
            walked: Default::default(),
            taken: Default::default(),
            rebased: Default::default(),
            locks: Default::default(),
            row_locks: Default::default(),
            unnamed: Default::default(),
            held: false,
            stale: false,
            reused: 0,
            declared: None,
            clock: (0, 0),
            handed: Default::default(),
            advancing: None,
            group: None,
            deciding: None,
            touched: Default::default(),
            pending: Default::default(),
            modes: Default::default(),
            applier: None,
            marking: None,
            functions: Default::default(),
            serial: Default::default(),
            ending: None,
            monitor: None,
            comparing: Default::default(),
            own: Default::default(),
            own_from: None,
            committing: false,
            first_bytes: Default::default(),
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
            snail.slot = crate::lock::take_slot(writer.file())?;
            snail.group = Some((std::rc::Rc::new(across::Group::alone(path, snail.slot)?), 0));
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

    /// Hands each commit from now on to the host before its root is written, which waits for the host
    /// to return (`carry`).
    pub fn carried_by(&mut self, host: Box<dyn crate::carry::Host>) -> Result<()> {
        match self.writer.as_mut() {
            Some(w) => {
                w.carried_by(host);
                Ok(())
            }
            None => Err(Error::new(
                "this snail was opened to read; open it to write",
            )),
        }
    }

    /// Opens a snail whose roots a host may have tagged, taking each only where `judge` says its
    /// host's transaction committed, as the round asking sees it. It holds no pages between rounds.
    pub fn open_judged(path: &Path, judge: std::rc::Rc<dyn crate::carry::Judge>) -> Result<Snail> {
        let reader = Reader::open(path)?;
        reader.judged_by(judge);
        Snail::opened(reader)
    }

    /// Opens a snail to write inside a host, as its one writer: every commit carried by `host`, a
    /// transaction's root tagged as the host tags it, and every root a host tagged taken only where
    /// `judge` says its transaction committed. Built with or without `standalone`, as inside
    /// PostgreSQL, whose commit is then the snail's: it holds no pages between rounds.
    pub fn open_carried(
        path: &Path,
        host: Box<dyn crate::carry::Host>,
        judge: std::rc::Rc<dyn crate::carry::Judge>,
    ) -> Result<Snail> {
        let (mut writer, _) = Writer::open(path)?;
        let mut snail = Snail::open_judged(path, judge)?;
        snail.slot = crate::lock::take_slot(writer.file())?;
        snail.group = Some((std::rc::Rc::new(across::Group::alone(path, snail.slot)?), 0));
        writer.carried_by(host);
        snail.writer = Some(writer);
        Ok(snail)
    }

    /// Writes from now on as the applier of `source`: on a snail marked a replica of `source`, the
    /// one writer it takes; on any other, a writer as any.
    pub fn applying(&mut self, source: &str) -> Result<()> {
        if self.writer.is_none() {
            return Err(Error::new(
                "this snail was opened to read; open it to write",
            ));
        }
        self.applier = Some(source.to_string());
        Ok(())
    }

    fn catalog(reader: &Reader, root: &Root) -> Result<Catalog> {
        let ddl = String::from_utf8(reader.read_extent(root.catalog)?)
            .map_err(|_| Error::new("the catalog is not UTF-8"))?;
        let mut catalog = Catalog::parse_kept(&ddl, usize::MAX)?;
        old_names(&mut catalog, reader.format());
        catalog.with_release(root.release)?;
        catalog.with_monetary(&root.monetary)?;
        Ok(catalog)
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

    /// The tag of a transaction whose commit the root the snail reads holds and which its judge's
    /// round does not see, where there is one: the generation that round sees is no longer in the
    /// file.
    pub fn unseen(&self) -> Option<u64> {
        self.reader.unseen()
    }

    /// The source the snail is marked a replica of, where it is marked one: as the root it reads
    /// records it, or as the transaction under way leaves it.
    pub fn replica(&self) -> Option<&str> {
        match &self.marking {
            Some(m) => m.as_deref(),
            None => self.root.replica.as_deref(),
        }
    }

    /// The PostgreSQL release the file's OIDs came from, whose catalog names its reg* values.
    pub fn release(&self) -> u8 {
        self.root.release
    }

    /// Each money column's `lc_monetary`, as the file records it: its table, its column and the
    /// locale.
    pub fn monetary(&self) -> &[(String, String, String)] {
        &self.root.monetary
    }

    /// The transactions across snails whose commit this snail's root wrote, each naming the root it
    /// wrote into each other snail: kept until a writer of them all finds every one written. Rows
    /// such a transaction wrote here may stand on the null side of a join with another snail until
    /// its root is written.
    pub fn decided(&self) -> &[crate::file::Decision] {
        &self.root.decided
    }

    /// Where each sequence stands, as the file holds it: its last value and whether it was handed
    /// out; where the file holds none, where its DDL leaves it.
    pub fn sequences(&self) -> Result<Vec<crate::file::SequenceState>> {
        let held = if self.root.sequences.bytes > 0 {
            crate::file::decode_sequences(&self.reader.read_extent(self.root.sequences)?)?
        } else {
            Vec::new()
        };
        Ok(self
            .catalog
            .sequences
            .iter()
            .map(|s| {
                held.iter().find(|h| h.0 == s.name).cloned().unwrap_or((
                    s.name.clone(),
                    s.last_value,
                    s.is_called,
                ))
            })
            .collect())
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

    /// The orders of a table that hold every column a question reads, by their place.
    fn serving(&self, want: &Want) -> Result<Vec<usize>> {
        let table = want.table;
        let mut needed = condition_columns(want.side);
        needed.extend(want.columns.iter().copied());
        if let Some(k) = want.keys {
            needed.extend(k.columns.iter().copied());
        }
        let mut served = Vec::new();
        for o in 0..table.orders.len() {
            if needed.iter().all(|&c| table.orders[o].slot(c).is_some()) {
                let predicate = partial::Predicate::of(&self.catalog, table, &table.orders[o])?;
                if predicate.is_none_or(|p| p.implied_by(want.side)) {
                    served.push(o);
                }
            }
        }
        Ok(served)
    }

    /// The fewest of what each order that serves a question says at its top it reads at most, and the
    /// directory pages that read.
    pub(super) fn most_at_tops(&self, want: &Want) -> Result<(u64, u64)> {
        let (mut most, mut read) = (u64::MAX, 0);
        for o in self.serving(want)? {
            let walk = self.start(o, want)?;
            most = most.min(walk.top.1);
            read += walk.directory_pages;
        }
        if most == u64::MAX {
            return Err(Error::new(format!(
                "no order of {} holds every column the question reads",
                want.table.name
            )));
        }
        Ok((most, read))
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
        // A partial strand holds only the rows its predicate admits, so it serves a question whose
        // conditions imply the predicate.
        let implied = |o: usize| -> Result<bool> {
            Ok(
                match partial::Predicate::of(&self.catalog, table, &table.orders[o])? {
                    Some(p) => p.implied_by(want.side),
                    None => true,
                },
            )
        };
        let mut served = Vec::new();
        for o in 0..table.orders.len() {
            if needed.iter().all(|&c| table.orders[o].slot(c).is_some()) && implied(o)? {
                served.push(o);
            }
        }
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
                if !implied(o)? {
                    return Err(Error::new(format!(
                        "{n} holds only the rows its predicate admits, WHERE {}, which the \
                         question does not imply",
                        table.orders[o].predicate.as_deref().unwrap_or_default()
                    )));
                }
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
        // The rows the question holds are the same along every order: each walk bounds them.
        let rows = walks.iter().fold((0, u64::MAX), |(lo, hi), w| {
            (
                lo.max(w.rows_closed + w.rows_low),
                hi.min(w.rows_closed + w.rows_high),
            )
        });
        let table_rows = walks.first().map_or(0, |w| w.table_rows);
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
            consulted.push(Consulted {
                order: name,
                directory_pages: plan.walk.directory_pages,
                low: plan.walk.low,
                high: plan.walk.high,
                exact: Some(plan.column_pages),
                held: plan.held,
            });
            plans.push(plan);
        }
        // The fewest pages from the file win; then the fewest touched, then the fewest directory pages.
        let best = (0..plans.len())
            .min_by_key(|&i| {
                let p = &plans[i];
                (
                    p.column_pages - p.held,
                    p.column_pages,
                    p.walk.directory_pages + p.map_pages,
                )
            })
            .unwrap();
        let plan = plans.swap_remove(best);
        // The order it reads goes on to read the maps of its blocks as it reaches them.
        let order_read = &table.orders[plan.walk.order].name;
        if let Some(c) = consulted.iter_mut().find(|c| &c.order == order_read) {
            c.directory_pages += plan.map_pages;
        }
        Ok(Part {
            plan,
            served: served
                .iter()
                .map(|&o| table.orders[o].name.clone())
                .collect(),
            consulted,
            rows,
            table_rows,
        })
    }

    /// A question on one table priced from its directory, on the root the snail last read: the order a
    /// round along it would read, its pages, and the rows the counts bound. It reads directory pages
    /// and page maps, never a column page.
    pub fn price(&self, sql: &str, order: Option<&str>) -> Result<Price> {
        let q = self.one_table(sql)?;
        let (want, _) = self.want_of(&q);
        let before = self.touched();
        let mut shown = want.table;
        let part = match self.along_elements(&q, &want, order)? {
            Along::Not => self.part(&want, order)?,
            Along::Table(part) => part,
            Along::Count(asked) => {
                shown = &self.catalog.tables[asked.sides[0].table];
                let w = Want {
                    table: shown,
                    side: &asked.sides[0],
                    columns: Vec::new(),
                    countable: asked.select.countable(),
                    keys: None,
                };
                self.part(&w, None)?
            }
            Along::Strands(chosen) => return self.price_along(&want, *chosen, before),
        };
        let row_bytes = row_bytes(&part.plan, want.table, want.columns.iter().copied());
        Ok(Price {
            row_bytes,
            order: shown.orders[part.plan.walk.order].name.clone(),
            served: part.served,
            consulted: part.consulted,
            table_rows: part.table_rows,
            rows_low: part.rows.0,
            rows_high: part.rows.1,
            rows_estimate: (part.plan.rows_estimate.round() as u64).clamp(part.rows.0, part.rows.1),
            keyless: part
                .plan
                .walk
                .texts
                .iter()
                .map(|&i| want.table.columns[want.side.texts[i].column].name.clone())
                .collect(),
            directory_pages: self.touched() - before,
            map_pages: part.plan.map_pages,
            column_pages: part.plan.column_pages,
            blocks: part.plan.blocks_read(),
        })
    }

    /// Begins a round on one table on the root the snail last read, its blocks handed out one at a
    /// time by `next_block`.
    pub fn scan(&self, sql: &str, order: Option<&str>) -> Result<Scan> {
        self.scan_question(self.one_table(sql)?, order)
    }

    fn scan_question(&self, q: Question, order: Option<&str>) -> Result<Scan> {
        let (want, columns) = self.want_of(&q);
        let (mut before, mut held_before) = (self.touched(), self.pages_held());
        let (part, kept, strands) = match self.along_elements(&q, &want, order)? {
            Along::Not => (self.part(&want, order)?, None, Round::default()),
            Along::Table(part) => (part, None, Round::default()),
            Along::Count(asked) => return self.scan_question(*asked, None),
            Along::Strands(chosen) => {
                let (kept, strands, _) = self.keys_of(want.table, &chosen)?;
                (before, held_before) = (self.touched(), self.pages_held());
                let w = Want {
                    table: want.table,
                    side: want.side,
                    columns: want.columns.clone(),
                    countable: want.countable.filter(|_| kept.is_none()),
                    keys: kept.as_ref(),
                };
                let part = self.part(&w, None)?;
                (part, kept, strands)
            }
        };
        self.mark_read(want.side.table, &part.plan.walk)?;
        let w = &part.plan.walk;
        let round = Round {
            order: want.table.orders[w.order].name.clone(),
            served: part.served.clone(),
            consulted: part.consulted.clone(),
            directory_pages: self.touched() - before,
            directory_pages_held: self.pages_held() - held_before,
            directory_pages_predicted: self.touched() - before + part.plan.map_pages,
            column_pages_top_low: w.top.0,
            column_pages_top_high: w.top.1,
            column_pages_low: w.low,
            column_pages_high: w.high,
            column_pages_predicted: part.plan.column_pages,
            entries: w.statuses.0 + w.statuses.1 + w.statuses.2,
            entries_none: w.statuses.0,
            entries_may: w.statuses.1,
            entries_must: w.statuses.2,
            blocks_read: part.plan.blocks_read(),
            ..Default::default()
        };
        // Ordered as the order it reads leads, each block's rows come in their order, those written
        // after its sorted ones put in their place: a block at a time.
        let o = &want.table.orders[w.order];
        let inputs = q.select.inputs();
        let led = q.order.len() <= o.levels.len()
            && q.order.iter().enumerate().all(|(i, s)| {
                inputs.get(s.at).is_some_and(|k| {
                    !matches!(k, Key::Month(_))
                        && o.levels[i] == crate::catalog::Level::Column(k.col().column)
                        && o.desc[i] == s.desc
                        && o.nulls_first[i] == s.nulls_first
                })
            });
        let whole =
            !(matches!(q.select, question::Select::Rows(_)) && (q.order.is_empty() || led));
        let counted = (w.counted, w.counted_months);
        Ok(Scan {
            q,
            kept,
            strands,
            plan: part.plan,
            columns,
            next: 0,
            whole,
            done: false,
            generation: self.root.generation,
            counted,
            round,
        })
    }

    /// Begins a round on one table, a question with no condition, that hands out `rows` of its live
    /// rows, each set of them as likely as any other by the counts, its blocks one at a time by
    /// `next_block`, reading only the column pages that hold them; with the live rows the table
    /// holds. `random` draws a number from 0 up to 1.
    pub fn sample(
        &self,
        sql: &str,
        rows: u64,
        random: &mut dyn FnMut() -> f64,
    ) -> Result<(Scan, u64)> {
        let mut scan = self.scan(sql, None)?;
        if !scan.q.sides[0].is_whole() {
            return Err(Error::new(format!(
                "a sample is drawn from a whole table: {sql}"
            )));
        }
        let live = scan.plan.live_rows();
        let (touched, held) = (self.touched(), self.pages_held());
        {
            let (want, _) = self.want_of(&scan.q);
            self.keep_rows(&mut scan.plan, &drawn(live, rows, random), &want)?;
        }
        scan.round.directory_pages += self.touched() - touched;
        scan.round.directory_pages_held += self.pages_held() - held;
        scan.round.directory_pages_predicted = scan.round.directory_pages + scan.plan.map_pages;
        scan.round.column_pages_predicted = scan.plan.column_pages;
        scan.round.blocks_read = scan.plan.blocks_read();
        Ok((scan, live))
    }

    /// The next block's rows of a round `scan` began, or none once every block has come. A question
    /// that groups, counts or orders its rows comes out in one batch.
    pub fn next_block(&self, scan: &mut Scan) -> Result<Option<RecordBatch>> {
        if scan.done {
            return Ok(None);
        }
        if scan.generation != self.root.generation {
            return Err(Error::new("a scan goes on only on the root it began on"));
        }
        let table = &self.catalog.tables[scan.q.sides[0].table];
        let want = Want {
            table,
            side: &scan.q.sides[0],
            columns: scan.columns.iter().map(|c| c.0).collect(),
            countable: scan.q.select.countable().filter(|_| scan.kept.is_none()),
            keys: scan.kept.as_ref(),
        };
        let blocks = scan.plan.blocks_read() as usize;
        let (read0, walked, taken) = (
            self.reader.pages_read(),
            self.walked.get(),
            self.taken.get(),
        );
        let maps0 = scan.plan.maps_read();
        let mut answer = Answer::new(&scan.q.select).typed(&[table]);
        let range = if scan.whole {
            scan.next..blocks
        } else {
            scan.next..(scan.next + 1).min(blocks)
        };
        if range.is_empty() && !scan.whole {
            scan.done = true;
            return Ok(None);
        }
        scan.next = range.end;
        self.scan_blocks(&scan.plan, &want, &scan.columns, &mut answer, range)?;
        if scan.whole {
            answer.counted(scan.counted.0, &scan.counted.1, scan.q.select.countable());
            scan.done = true;
        }
        let batch = answer.finish(&[table], &scan.q.select, &scan.q.order)?;
        let r = &mut scan.round;
        let maps = scan.plan.maps_read();
        r.column_pages_read += self.reader.pages_read() - read0 - (maps.file - maps0.file);
        r.column_pages_held = scan.plan.held_read();
        r.directory_pages += maps.touched - maps0.touched;
        r.directory_pages_held += maps.held - maps0.held;
        r.rows += batch.num_rows() as u64;
        r.rows_walked += self.walked.get() - walked;
        r.rows_taken += self.taken.get() - taken;
        Ok(Some(batch))
    }

    /// Answers a question on one table as `ask` does, each block's rows handed to `each` as they are
    /// read: a question that groups, counts or orders its rows hands over one batch at the end.
    pub fn ask_each(
        &mut self,
        sql: &str,
        order: Option<&str>,
        each: &mut dyn FnMut(RecordBatch) -> Result<()>,
    ) -> Result<Round> {
        let pages_at_start = self.begin_round()?;
        let _clocked = crate::scalars::clocked(self.clock.0, self.clock.1);
        let mut scan = self.scan(sql, order)?;
        while let Some(batch) = self.next_block(&mut scan)? {
            each(batch)?;
        }
        let mut round = scan.round();
        round.pages_at_start = pages_at_start;
        self.serial_alone_ends()?;
        Ok(round)
    }

    /// Begins a round: it reads what was committed when it begins; in a transaction, with the
    /// transaction's own changes in place. The pages it read to begin.
    fn begin_round(&mut self) -> Result<u64> {
        if self.aborted {
            return Err(Error::new(change::ABORTED));
        }
        let begun = self.begun.is_some();
        let pages_at_start = if self.writer.is_some() && self.levels.serializable(begun) {
            let before = self.reader.pages_read();
            self.serial_snapshot(begun)?;
            self.held = begun;
            self.reader.pages_read() - before
        } else if self.rounds > 0 && self.begun.is_none() {
            self.renew()?
        } else if self.rounds > 0 && !self.held {
            let pages = self.renew_in_transaction()?;
            // At REPEATABLE READ, the generation the first read takes is the transaction's.
            self.held = self.levels.repeatable(true);
            pages
        } else {
            0
        };
        self.rounds += 1;
        // Its clock is its transaction's, begun at BEGIN or with the round alone.
        let at = change::wall_micros();
        self.clock.1 = at;
        if self.begun.is_none() {
            self.clock.0 = at;
        } else {
            self.levels.read();
        }
        Ok(pages_at_start)
    }

    /// A question on one table, refused as anything else.
    fn one_table(&self, sql: &str) -> Result<Question> {
        let q = question::parse(&self.catalog, sql)?;
        if q.sides.len() != 1 {
            return Err(Error::new(
                "a question on two tables is not priced or scanned alone",
            ));
        }
        Ok(q)
    }

    /// What a round on one table reads: the columns of the answer's inputs, each with whether it is
    /// read for its month.
    fn want_of<'a>(&'a self, q: &'a Question) -> (Want<'a>, Vec<(usize, bool)>) {
        let table = &self.catalog.tables[q.sides[0].table];
        let columns: Vec<(usize, bool)> = q
            .select
            .inputs()
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
        (want, columns)
    }

    /// Answers a question, along the order named or the one whose pages are fewest: one round.
    pub fn ask(&mut self, sql: &str, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        self.ask_with(sql, order, &[])
    }

    /// Gives each round from now on the room `sizes` names: the bytes it holds its rows and the pages
    /// it keeps for its later reads in, 128 MB unless told otherwise; the directory it writes the rows
    /// past them to, a file of its own removed as soon as it is opened; and the most it writes there.
    pub fn size_rounds(&mut self, sizes: crate::room::Sizes) {
        self.reader.room().set_sizes(sizes);
    }

    /// The room each round is given.
    pub fn round_sizes(&self) -> crate::room::Sizes {
        self.reader.room().sizes()
    }

    /// Hands the snail a function of its caller's, which its questions and statements then call by
    /// its name as any function: refused where PostgreSQL or the snail's DDL has a function of its
    /// name, or a form of the same arguments is handed already.
    pub fn hand(&mut self, f: handed::Function) -> Result<()> {
        handed::hand(&mut self.functions, &self.catalog, f)
    }

    /// Answers a question with its parameters, `$1` and on, given as text: one round.
    pub fn ask_with(
        &mut self,
        sql: &str,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let asked = self.round_asked(sql, order, params);
        // At SERIALIZABLE a round that fails fails its transaction.
        let begun = self.begun.is_some();
        match &asked {
            Err(_) if self.levels.serializable(begun) && self.writer.is_some() => {
                if !self.aborted {
                    self.abandon()?;
                    self.aborted = begun;
                }
            }
            Err(_) => {}
            Ok(_) => self.serial_alone_ends()?,
        }
        asked
    }

    fn round_asked(
        &mut self,
        sql: &str,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let pages_at_start = self.begin_round()?;
        let _clocked = crate::scalars::clocked(self.clock.0, self.clock.1);
        let _handed = handed::holding(&self.functions);
        let room = self.reader.room();
        let _room = self.entered_round(&room);
        self.wasted.borrow_mut().clear();
        let (batch, mut round) = match locking::clauses(sql)? {
            Some(l) => self.ask_locking(l, order, params)?,
            None => self.answer_with(sql, order, params)?,
        };
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
        let room = self.reader.room();
        let entered = self.entered_round(&room);
        let (batch, mut round) = self.answer_in_room(sql, order, params)?;
        let room = crate::room::current().expect("a round under way has its room");
        round.bytes_held = room.most();
        (round.spill_pages_written, round.spill_pages_read) = room.spill_pages();
        drop(entered);
        Ok((batch, round))
    }

    fn answer_in_room(
        &self,
        sql: &str,
        order: Option<&str>,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        question::unlocked_words(sql)?;
        relations::unread_clauses(sql)?;
        let sql = &*spelled::spelled(sql)?;
        let parsed =
            sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, sql)?;
        if let [sqlparser::ast::Statement::Query(q)] = parsed.as_slice() {
            question::unlocked(q)?;
            if let Some(r) = computed(&self.catalog, q)? {
                if order.is_some() {
                    return Err(Error::new(
                        "a question that reads a virtual column reads its table along its own order",
                    ));
                }
                return self.ask_pipeline(&r, params);
            }
            // A subquery is read by the pipeline over several relations.
            if subqueries::held(q) {
                if order.is_some() {
                    return Err(Error::new(
                        "a question holding a subquery chooses its orders itself",
                    ));
                }
                return self.ask_pipeline(q, params);
            }
            if pipeline::is_pipeline(q)
                || relations::is_set_operation(q)
                || pipeline::tests_composite_null(&self.catalog, q)
                || pipeline::reads_per_row(&self.catalog, q)
            {
                if order.is_some() && (pipeline::is_pipeline(q) || relations::is_set_operation(q)) {
                    return Err(Error::new("a pipeline reads its table along its own order"));
                }
                return self.ask_pipeline(q, params);
            }
            // What the question path does not read, a pipeline reads: a view, and parameters, among
            // them.
            if relations::names_a_view(&self.catalog, q)
                || !params.is_empty()
                || pipeline::hands_over(q) && question::parse(&self.catalog, sql).is_err()
                || elements::read_a_row_at_a_time(&self.catalog, q)
                    && question::parse(&self.catalog, sql).is_err()
            {
                if order.is_some() {
                    return Err(Error::new("a pipeline reads its table along its own order"));
                }
                return self.ask_pipeline(q, params);
            }
        }
        if !params.is_empty() {
            return Err(Error::new(
                "parameters are read only in a question on one table",
            ));
        }
        let q = question::parse(&self.catalog, sql)?;
        match q.sides.len() {
            1 => self.ask_one(&q, order),
            _ if order.is_some() => Err(Error::new("a join chooses its orders itself")),
            _ => match (self.ask_join(&q), parsed.as_slice()) {
                (Err(e), [sqlparser::ast::Statement::Query(q)]) if join::past_the_room(&e) => {
                    self.ask_pipeline(q, params)
                }
                (r, _) => r,
            },
        }
    }

    /// A question read as a pipeline: its one table read whole along its own order, then its steps;
    /// one over several relations, each step's FROM met as one.
    fn ask_pipeline(
        &self,
        q: &sqlparser::ast::Query,
        params: &[String],
    ) -> Result<(RecordBatch, Round)> {
        let _rows = handed::rows_of(&self.catalog);
        let q = &pipeline::windows::written(&spelled::between_written(q))?;
        if relations::several(&self.catalog, q) || subqueries::held(q) {
            return relations::answer(self, q, params);
        }
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
        // The columns it stores: a step computes a virtual one from them.
        let stored: Vec<usize> = (0..table.columns.len())
            .filter(|&c| !table.columns[c].is_virtual())
            .collect();
        let columns = stored
            .iter()
            .map(|&c| format!("\"{}\"", table.columns[c].name.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(", ");
        let base = question::parse(
            &self.catalog,
            &format!("SELECT {columns} FROM {}", table.name),
        )?;
        // The table's order carries its windows where it is one level, unique, every row its own
        // peer: its key's column, NOT NULL, or holding at most one NULL, last.
        let along = match (order.unique, order.levels.as_slice()) {
            (true, [crate::catalog::Level::Column(c)])
                if table.columns[*c].not_null
                    || (!order.nulls_first[0] && !order.nulls_distinct) =>
            {
                stored.iter().position(|x| x == c)
            }
            _ => None,
        };
        let names = stored
            .iter()
            .map(|&c| table.columns[c].name.clone())
            .collect();
        let types = stored.iter().map(|&c| Some(table.columns[c].ty)).collect();
        let primary: Vec<String> = table
            .orders
            .iter()
            .find(|o| o.constraint.as_ref().is_some_and(|k| k.primary))
            .map(|o| {
                o.levels
                    .iter()
                    .map(|l| table.columns[l.column()].name.clone())
                    .collect()
            })
            .unwrap_or_default();
        // A strand the question's windows read along, a partition at a time, where its pages are
        // fewer than the table's own order's with the sort it saves; the rows of each block in
        // their place among its sorted rows.
        let priced = (self.touched(), self.pages_held());
        let strand = match strands::of_step(q, table, &stored) {
            Some(s) if self.strand_wins(&base, table, s.0, Some(&order.name))? => Some(s),
            _ => None,
        };
        let priced = (self.touched() - priced.0, self.pages_held() - priced.1);
        let along = along.filter(|_| strand.is_none());
        let mut stream = pipeline::Stream::new(
            q,
            &table.name,
            names,
            types,
            along,
            &primary,
            params,
            table.release,
            pipeline::Casings::of_table(table, &self.catalog.collations),
        )?;
        if stream.holds_steps() {
            drop(stream);
            return relations::answer(self, q, params);
        }
        if let Some((_, _, led)) = &strand {
            stream.along(*led);
        }
        let sorted_at = strand.as_ref().map(|(_, at, _)| at.clone());
        // Each block's rows go to the pipeline as the scan reads them, in the order.
        let most = std::cell::Cell::new(None);
        let mut told = false;
        let mut blocks = Blocks {
            table,
            q: &base,
            answer: Answer::new(&base.select),
            most: Some(&most),
            feed: |batch: RecordBatch| {
                if !told {
                    told = true;
                    if let Some(n) = most.get() {
                        stream.expect_rows(n);
                    }
                }
                let mut rel = pipeline::relation_of(&batch)?;
                if let Some(c) = along {
                    pipeline::in_order(&mut rel, c)?;
                }
                if let Some(at) = &sorted_at {
                    pipeline::in_place(&mut rel.rows, at)?;
                }
                stream.feed(rel)
            },
        };
        let read_along = match &strand {
            Some((o, _, _)) => &table.orders[*o].name,
            None => &order.name,
        };
        let (mut round, _, _) = self.scan_one(&base, Some(read_along), None, &mut blocks)?;
        round.directory_pages += priced.0;
        round.directory_pages_predicted += priced.0;
        round.directory_pages_held += priced.1;
        drop(blocks);
        round.rows_held = stream.most_held() as u64;
        let out = stream.finish()?;
        let batch = pipeline::batch_of(&out)?;
        round.rows = batch.num_rows() as u64;
        Ok((batch, round))
    }

    fn ask_one(&self, q: &Question, order: Option<&str>) -> Result<(RecordBatch, Round)> {
        self.ask_one_kept(q, order, None)
    }

    fn ask_one_kept(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
    ) -> Result<(RecordBatch, Round)> {
        let table = &self.catalog.tables[q.sides[0].table];
        let mut answer = Answer::new(&q.select).typed(&[table]);
        let (mut round, counted, months) = self.scan_one(q, order, kept, &mut answer)?;
        answer.counted(counted, &months, q.select.countable());
        let batch = answer.finish(&[table], &q.select, &q.order)?;
        round.rows = batch.num_rows() as u64;
        Ok((batch, round))
    }

    /// A question of rows on one table, kept to the keys `kept` holds where it is given them: the
    /// round along the order whose pages are fewest, those keys known before it is priced, its rows
    /// handed to `each` a block at a time as they are read.
    pub(super) fn read_blocks(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
        most: &std::cell::Cell<Option<u64>>,
        each: &mut dyn FnMut(Vec<Vec<Value>>) -> Result<()>,
    ) -> Result<Round> {
        let table = &self.catalog.tables[q.sides[0].table];
        let mut rows = 0u64;
        let mut blocks = Blocks {
            table,
            q,
            answer: Answer::new(&q.select).typed(&[table]),
            most: Some(most),
            feed: |batch: RecordBatch| {
                rows += batch.num_rows() as u64;
                each(pipeline::relation_of(&batch)?.rows)
            },
        };
        let (mut round, _, _) = self.scan_one(q, order, kept, &mut blocks)?;
        drop(blocks);
        round.rows = rows;
        Ok(round)
    }

    /// A question on one table read along an order into a sink: the round, and what the directory
    /// counted, in all and by month.
    fn scan_one(
        &self,
        q: &Question,
        order: Option<&str>,
        kept: Option<&KeySet>,
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
            countable: q.select.countable().filter(|_| kept.is_none()),
            keys: kept,
        };
        let (before, held_before) = (self.touched(), self.pages_held());
        let along = match kept {
            None => self.along_elements(q, &want, order)?,
            Some(_) => Along::Not,
        };
        let part = match along {
            Along::Not => self.part(&want, order)?,
            Along::Table(part) => part,
            Along::Count(asked) => return self.scan_one(&asked, None, None, sink),
            Along::Strands(chosen) => {
                let (keys, mut strands, _held) = self.keys_of(table, &chosen)?;
                let want = Want {
                    table,
                    side: want.side,
                    columns: want.columns,
                    countable: want.countable.filter(|_| keys.is_none()),
                    keys: keys.as_ref(),
                };
                let (before, held_before) = (self.touched(), self.pages_held());
                let part = self.part(&want, None)?;
                let (r, counted, months) =
                    self.scan_part(&want, &columns, part, sink, before, held_before)?;
                strands.add_table(&r, &r.order);
                strands.served = r.served.clone();
                strands.rows_walked += r.rows_walked;
                strands.rows_taken += r.rows_taken;
                strands.reads.push(r);
                return Ok((strands, counted, months));
            }
        };
        self.scan_part(&want, &columns, part, sink, before, held_before)
    }

    /// A table's read along the order priced: the round, and what the directory counted, in all and
    /// by month.
    fn scan_part(
        &self,
        want: &Want,
        columns: &[(usize, bool)],
        part: Part,
        sink: &mut dyn Sink,
        before: u64,
        held_before: u64,
    ) -> Result<(Round, u64, [u64; 12])> {
        let table = want.table;
        self.mark_read(want.side.table, &part.plan.walk)?;
        let directory_pages = self.touched() - before;
        let directory_pages_held = self.pages_held() - held_before;
        self.keep_held(&part.plan, want)?;
        let mid = self.reader.pages_read();
        let (walked, taken) = (self.walked.get(), self.taken.get());
        sink.rows_most(part.rows.1);
        let read = self.read_plan(&part.plan, want, columns, sink);
        self.reader.read_done();
        let maps = part.plan.maps_read();
        let column_pages_read = self.reader.pages_read() - mid - maps.file;
        let column_pages_held = part.plan.held_read();
        let directory_pages_predicted = directory_pages + part.plan.map_pages;
        let directory_pages = directory_pages + maps.touched;
        let directory_pages_held = directory_pages_held + maps.held;
        let w = &part.plan.walk;
        if let Err(e) = read {
            if join::past_the_room(&e) {
                join::stop_with(Round {
                    order: table.orders[w.order].name.clone(),
                    directory_pages,
                    directory_pages_held,
                    directory_pages_predicted: directory_pages,
                    column_pages_predicted: column_pages_read,
                    column_pages_held,
                    column_pages_read,
                    ..Default::default()
                });
            }
            return Err(e);
        }
        let round = Round {
            order: table.orders[w.order].name.clone(),
            served: part.served.clone(),
            consulted: part.consulted.clone(),
            pages_at_start: 0,
            directory_pages,
            directory_pages_held,
            directory_pages_predicted,
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
            snails: Vec::new(),
            ..Default::default()
        };
        Ok((round, w.counted, w.counted_months))
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
        // Where the sequences stand, once one has handed out a value.
        if self.root.sequences.bytes > 0 {
            mark(self.root.sequences, 1)?;
        }
        // The serializable transactions' records, while a round older than their commits runs.
        if self.root.serial.bytes > 0 {
            mark(self.root.serial, 1)?;
        }
        // The roots each decision across snails carries.
        for d in &self.root.decided {
            if d.carried.bytes > 0 {
                mark(d.carried, 1)?;
            }
        }
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
                        below.extend(decode_page(&self.reader.read_extent(e.child)?, &head, e.bound())?);
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
                        below.extend(decode_page(&self.reader.read_extent(e.child)?, &head, e.bound())?);
                    }
                    level = below;
                }
                let sublevels = Sublevels::new(order, table, &head.level_text);
                for (bi, b) in level.iter().enumerate() {
                    let map = read_map(&self.reader, b, &head.level_text)?;
                    // The block's first key, as its first page names it: an entry may keep only the
                    // separator below it.
                    let first = map
                        .columns
                        .first()
                        .and_then(|c| c.pages.first())
                        .map_or(&b.first, |p| &p.first);
                    let mut place = sublevels.all(first);
                    place.truncate(b.depth as usize);
                    let (map_run, _) = b.map_run();
                    for (s, (c, leaf)) in order.leaves(table).iter().enumerate() {
                        let cp = &map.columns[s];
                        out.push(BlockRow {
                            table: table.name.clone(),
                            order: order.name.clone(),
                            block: bi as u64,
                            depth: b.depth as u64,
                            place: format!("{:?}", place),
                            first: shown(first),
                            last: shown(&walk::narrowed(b, order, table).1),
                            rows: b.rows as u64,
                            map_pages: map_run.pages(),
                            column: leaf.name(&table.columns[*c].name),
                            k: if cp.unit != 0 { cp.unit } else { head.units[s] } as u64,
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
    /// Told the rows the counts say the round may give, before its first block.
    most: Option<&'a std::cell::Cell<Option<u64>>>,
    feed: F,
}

impl<F: FnMut(RecordBatch) -> Result<()>> Sink for Blocks<'_, F> {
    fn rows_most(&mut self, n: u64) {
        if let Some(m) = self.most {
            m.set(Some(n));
        }
    }

    fn dictionaries(&mut self, dicts: &[Dict]) {
        self.answer.dictionaries(dicts);
    }

    fn row(&mut self, cells: &[Cell]) {
        self.answer.add(cells);
    }

    fn block_end(&mut self) -> Result<()> {
        // The scan sends each block's own dictionaries before the block's first row.
        let done = std::mem::replace(
            &mut self.answer,
            Answer::new(&self.q.select).typed(&[self.table]),
        );
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
#[non_exhaustive]
pub struct BlockRow {
    pub table: String,
    pub order: String,
    pub block: u64,
    pub depth: u64,
    pub place: String,
    /// The block's first key.
    pub first: String,
    /// Its last key; where its order keeps one separator between blocks, the greatest key the
    /// separator above it leaves it.
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
