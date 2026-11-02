//! A question ordered by its distance from a point, read along a strand on the plane: the strand's
//! blocks nearest first by the least distance from the point to each block's box, each read's pages
//! known before it is read, stopping once the rows asked for lie nearer than the next block's box. A
//! column the strand does not hold is read after, the table kept to the keys of the rows found. Where
//! the table's own read and its sort come to no more pages, the question is left to them.

use super::answer::Answer;
use super::pipeline::{self, Casings, Relation, Stream, Value};
use super::question::Question;
use super::relations::{self, Keep, Kept};
use super::spill::{row_width, Run, CHUNK};
use super::strands::{held_width, Led};
use super::walk::{Best, Want};
use super::{order_tag, spelled, Blocks, Consulted, Round, Snail};
use crate::catalog::{Catalog, Method, Table};
use crate::cut::PAGE;
use crate::geo::{Geo, GeoKind};
use crate::plane::Near;
use crate::room::Room;
use crate::{Error, Result};
use arrow::array::RecordBatch;
use sqlparser::ast::{
    Expr, GroupByExpr, Ident, OrderByKind, OrderBySort, Query, Select, SelectItem,
    SelectItemQualifiedWildcardKind, SetExpr, TableFactor, Value as SqlValue,
};
use std::collections::BTreeMap;
use std::rc::Rc;

/// What became of a question asked of the plane's distances.
pub(super) enum Nearest {
    /// It orders by no distance a strand can give, or names an order the walk does not read.
    Not,
    /// The table's own read and its sort are the fewer pages: the directory pages and those of them
    /// found held the pricing touched.
    Table(u64, u64),
    Answered(RecordBatch, Round),
}

/// What a question orders by: its table and the name its FROM gives it, the column of the plane, the
/// value it stands from, the distance as written, and whether its ORDER BY goes on past that.
struct Asked<'q> {
    table: usize,
    qualifier: String,
    column: usize,
    value: &'q Expr,
    distance: &'q Expr,
    more: bool,
}

/// What an item of an ORDER BY sorts by: an item of the select list, by its alias or its place.
fn sorted_by<'q>(e: &'q Expr, s: &'q Select) -> &'q Expr {
    let item = |i: &'q SelectItem| match i {
        SelectItem::UnnamedExpr(x) | SelectItem::ExprWithAlias { expr: x, .. } => Some(x),
        _ => None,
    };
    match e {
        Expr::Nested(x) => sorted_by(x, s),
        Expr::Identifier(i) => s
            .projection
            .iter()
            .find_map(|p| match p {
                SelectItem::ExprWithAlias { expr, alias }
                    if crate::catalog::ident(alias) == crate::catalog::ident(i) =>
                {
                    Some(expr)
                }
                _ => None,
            })
            .map_or(e, |x| sorted_by(x, s)),
        Expr::Value(v) => match &v.value {
            SqlValue::Number(n, _) => n
                .parse::<usize>()
                .ok()
                .and_then(|k| s.projection.get(k.checked_sub(1)?))
                .and_then(item)
                .map_or(e, |x| match x {
                    Expr::Value(_) => x,
                    x => sorted_by(x, s),
                }),
            _ => e,
        },
        _ => e,
    }
}

/// Whether an expression comes to one value whatever the row: no column, and nothing a statement
/// draws.
fn fixed(e: &Expr) -> bool {
    match e {
        Expr::Value(_) | Expr::TypedString(_) => true,
        Expr::Nested(x) | Expr::Cast { expr: x, .. } | Expr::UnaryOp { expr: x, .. } => fixed(x),
        Expr::BinaryOp { left, right, .. } => fixed(left) && fixed(right),
        Expr::Function(f) if crate::catalog::object_name(&f.name).to_lowercase() == "point" => {
            pipeline::parts_of(e).into_iter().all(fixed)
        }
        _ => false,
    }
}

/// What a question asks of a distance: one SELECT on one table, ordered first by a column of the
/// plane's distance from a value, rising, its NULLs last. None for any other.
fn asked<'q>(catalog: &Catalog, q: &'q Query) -> Option<Asked<'q>> {
    let Some(OrderByKind::Expressions(os)) = q.order_by.as_ref().map(|o| &o.kind) else {
        return None;
    };
    let first = os.first()?;
    if q.with.is_some()
        || matches!(first.options.sort, Some(OrderBySort::Desc))
        || first.options.nulls_first == Some(true)
    {
        return None;
    }
    let SetExpr::Select(s) = q.body.as_ref() else {
        return None;
    };
    let [from] = s.from.as_slice() else {
        return None;
    };
    let plain = matches!(&s.group_by, GroupByExpr::Expressions(g, _) if g.is_empty());
    if !plain || !from.joins.is_empty() || s.distinct.is_some() || s.having.is_some() {
        return None;
    }
    let TableFactor::Table {
        name,
        alias,
        args: None,
        ..
    } = &from.relation
    else {
        return None;
    };
    let n = crate::catalog::object_name(name);
    let table = catalog
        .tables
        .iter()
        .position(|t| t.name == n || t.name.rsplit('.').next() == Some(n.as_str()))?;
    let qualifier = match alias {
        Some(a) => crate::catalog::ident(&a.name),
        None => n.rsplit('.').next().unwrap_or(&n).to_string(),
    };
    let distance = sorted_by(&first.expr, s);
    let Expr::BinaryOp { left, op, right } = distance else {
        return None;
    };
    if op.to_string() != "<->" {
        return None;
    }
    let t = &catalog.tables[table];
    let column_of = |e: &Expr| {
        let (by, name) = pipeline::column_of(e)?;
        if by.is_some_and(|b| b != qualifier) {
            return None;
        }
        t.columns.iter().position(|c| c.name == name)
    };
    let (column, value) = match (column_of(left), column_of(right)) {
        (Some(c), None) => (c, &**right),
        (None, Some(c)) => (c, &**left),
        _ => return None,
    };
    if !crate::plane::on_plane(t.columns[column].ty) || !fixed(value) {
        return None;
    }
    Some(Asked {
        table,
        qualifier,
        column,
        value,
        distance,
        more: os.len() > 1,
    })
}

/// The question with each star of its select list written as the table's columns; none where a star
/// says anything more, or names another relation.
fn stars_written(q: &Query, table: &Table, qualifier: &str) -> Option<Query> {
    let mut q = q.clone();
    let SetExpr::Select(s) = q.body.as_mut() else {
        return None;
    };
    let columns = || {
        relations::stored(table).map(|c| {
            SelectItem::UnnamedExpr(Expr::Identifier(Ident::with_quote(
                '"',
                table.columns[c].name.clone(),
            )))
        })
    };
    let mut out = Vec::with_capacity(s.projection.len());
    for item in s.projection.drain(..) {
        match item {
            SelectItem::Wildcard(o) if o.to_string().is_empty() => out.extend(columns()),
            SelectItem::QualifiedWildcard(SelectItemQualifiedWildcardKind::ObjectName(n), o)
                if o.to_string().is_empty()
                    && crate::catalog::object_name(&n) == qualifier =>
            {
                out.extend(columns());
            }
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => return None,
            other => out.push(other),
        }
    }
    s.projection = out;
    Some(q)
}

/// A distance as it sorts; refused where the expression came to no distance.
fn near_of(v: &Value) -> Result<Near> {
    match v {
        Value::Null => Ok(Near::NULL),
        Value::Double(d) => Ok(Near::of(*d)),
        other => Err(Error::new(format!("a distance came to {other:?}"))),
    }
}

/// The rows a walk has read and not yet handed on, nearest first, in the round's room: where they do
/// not fit, those that sort last are written, and read back once the rows before them have gone.
struct Frontier {
    room: Option<Rc<Room>>,
    rows: BTreeMap<(Near, u64), Vec<Value>>,
    bytes: u64,
    past: Option<Run>,
    past_least: Option<Near>,
    seq: u64,
    most: usize,
}

impl Frontier {
    fn new() -> Frontier {
        Frontier {
            room: crate::room::current(),
            rows: BTreeMap::new(),
            bytes: 0,
            past: None,
            past_least: None,
            seq: 0,
            most: 0,
        }
    }

    /// Writes a row, its distance beside it.
    fn write(&mut self, at: Near, mut row: Vec<Value>) -> Result<()> {
        row.push(at.distance().map_or(Value::Null, Value::Double));
        self.past.get_or_insert_with(Run::past).push(row)?;
        self.past_least = Some(self.past_least.map_or(at, |p| p.min(at)));
        Ok(())
    }

    /// A row more: held where the room has room for it, or while it holds less than a chunk; else the
    /// greatest of those held written to make room where its own is less, else itself.
    fn push(&mut self, at: Near, row: Vec<Value>) -> Result<()> {
        let bytes = row_width(&row);
        self.seq += 1;
        if let Some(room) = self.room.clone() {
            loop {
                if room.hold(bytes) {
                    break;
                }
                if self.bytes + bytes <= CHUNK {
                    room.hold_anyway(bytes);
                    break;
                }
                let least_past = self.rows.last_key_value().is_some_and(|(k, _)| k.0 > at);
                if !least_past {
                    return self.write(at, row);
                }
                let (k, out) = self.rows.pop_last().expect("a row is held");
                let width = row_width(&out);
                self.bytes -= width;
                room.let_go(width);
                self.write(k.0, out)?;
            }
        }
        self.bytes += bytes;
        self.rows.insert((at, self.seq), row);
        self.most = self.most.max(self.rows.len());
        Ok(())
    }

    /// The rows that sort before `next`, or with it where no row of its distance need follow them,
    /// none past what was written, and no more than `most` but those a row before them ties; with the
    /// distance of the last, `last` where it hands on none.
    fn release(
        &mut self,
        next: Near,
        strict: bool,
        most: Option<u64>,
        mut last: Option<Near>,
    ) -> (Vec<Vec<Value>>, Option<Near>) {
        let mut out = Vec::new();
        while let Some((&(at, _), _)) = self.rows.first_key_value() {
            let before = at < next || (!strict && at == next);
            if !before || self.past_least.is_some_and(|p| at > p) {
                break;
            }
            let enough = most.is_some_and(|m| out.len() as u64 >= m);
            if enough && !(strict && last == Some(at)) {
                break;
            }
            let (_, row) = self.rows.pop_first().expect("a row is held");
            let width = row_width(&row);
            self.bytes -= width;
            if let Some(room) = &self.room {
                room.let_go(width);
            }
            out.push(row);
            last = Some(at);
        }
        (out, last)
    }

    /// The nearest it holds or wrote.
    fn nearest(&self) -> Near {
        let held = self.rows.first_key_value().map_or(Near::PAST, |(k, _)| k.0);
        held.min(self.past_least.unwrap_or(Near::PAST))
    }

    /// What was written, read back.
    fn read_back(&mut self) -> Result<()> {
        let Some(past) = self.past.take() else {
            return Ok(());
        };
        self.past_least = None;
        past.seal()?;
        past.each_chunk(&mut |chunk| {
            for mut row in chunk {
                let at = near_of(&row.pop().expect("a row written keeps its distance"))?;
                self.push(at, row)?;
            }
            Ok(())
        })
    }
}

/// What a walk read.
#[derive(Default)]
struct Walked {
    touched: u64,
    touched_held: u64,
    predicted: u64,
    column_predicted: u64,
    column_held: u64,
    column_read: u64,
    blocks: u64,
    rows_held: usize,
}

/// A question's own columns on the strand: the values of its rows at their places among the table's.
struct Columns<'a> {
    at: &'a [usize],
    width: usize,
}

impl Columns<'_> {
    fn row(&self, values: Vec<Value>) -> Vec<Value> {
        let mut row = vec![Value::Null; self.width];
        for (v, &p) in values.into_iter().zip(self.at) {
            row[p] = v;
        }
        row
    }
}

/// How a walk's rows are handed on: to the question's stream, or their keys kept for the table's read.
enum Hand<'a> {
    Stream(Vec<String>),
    Keys(&'a [usize], &'a mut Vec<Vec<Value>>),
}

impl Snail {
    /// A question asked of a distance, along the strand named or the one whose pages are fewest: its
    /// answer, or what leaves it to the rest of the read.
    pub(super) fn ask_nearest(
        &self,
        q: &Query,
        order: Option<&str>,
        params: &[String],
    ) -> Result<Nearest> {
        let Some(a) = asked(&self.catalog, q) else {
            return Ok(Nearest::Not);
        };
        let table = &self.catalog.tables[a.table];
        let Some(q) = stars_written(q, table, &a.qualifier) else {
            return Ok(Nearest::Not);
        };
        let q = &pipeline::windows::written(&spelled::between_written(&q))?;
        let Some(a) = asked(&self.catalog, q) else {
            return Ok(Nearest::Not);
        };
        let Some(own) = table.orders.iter().position(|o| o.table_order) else {
            return Ok(Nearest::Not);
        };
        let SetExpr::Select(s) = q.body.as_ref() else {
            return Ok(Nearest::Not);
        };
        let stored: Vec<usize> = relations::stored(table).collect();
        let names: Vec<String> = stored
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
        let casings = Casings::of_table(table, &self.catalog.collations);
        let stream = Stream::new(
            q,
            &table.name,
            names.clone(),
            types,
            None,
            &primary,
            params,
            table.release,
            casings,
        );
        let Ok(mut stream) = stream else {
            return Ok(Nearest::Not);
        };
        if !stream.one_row_each() {
            return Ok(Nearest::Not);
        }
        let kind = match table.columns[a.column].ty {
            crate::catalog::ColumnType::Geometric(k) => k,
            _ => return Ok(Nearest::Not),
        };
        let is_strand = |o: usize| {
            let order = &table.orders[o];
            matches!(order.method, Method::Gist | Method::Spgist)
                && crate::spans::columns(table, order).contains(&a.column)
        };
        match order {
            Some(n) if table.orders[own].name == n && !is_strand(own) => {
                return Ok(Nearest::Table(0, 0))
            }
            Some(n) if !table.orders.iter().any(|o| o.name == n) => return Ok(Nearest::Not),
            _ => {}
        }
        let point = match stream.constant(a.value) {
            Ok(Value::Geometric(Geo::Point(p))) => p,
            Ok(Value::Text(t)) if kind == GeoKind::Point => match crate::geo::geo_in(kind, &t) {
                Ok(Geo::Point(p)) => p,
                _ => return Ok(Nearest::Not),
            },
            _ => return Ok(Nearest::Not),
        };
        let Ok((reached, ties)) = stream.rows_reached() else {
            return Ok(Nearest::Not);
        };
        if !point.x.is_finite() || !point.y.is_finite() {
            return Ok(Nearest::Not);
        }
        let strict = a.more || ties;
        let named = |o: usize| order.is_none_or(|n| table.orders[o].name == n);

        // What the question can mean of the table, on the conditions the question path reads.
        let (before, held_before) = (self.touched(), self.pages_held());
        let words = relations::Named::of(&q.to_string());
        let wanted: Vec<usize> = stored
            .iter()
            .copied()
            .filter(|&c| words.names(Some(&a.qualifier), &table.columns[c].name))
            .collect();
        let mut conds = Vec::new();
        if let Some(w) = &s.selection {
            relations::conjuncts(w, &mut conds);
        }
        conds.retain(|c| !c.to_string().contains('$'));
        let from = format!(
            "{} {}",
            relations::quote_name(&table.name),
            relations::quote(&a.qualifier)
        );
        let select = |cs: &[usize]| {
            cs.iter()
                .map(|&c| {
                    format!(
                        "{}.{}",
                        relations::quote(&a.qualifier),
                        relations::quote(&table.columns[c].name)
                    )
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        let Ok(asked_wanted) = relations::parsed(&self.catalog, &select(&wanted), &from, &conds)
        else {
            return Ok(Nearest::Not);
        };
        let strands_of = |want: &Want| -> Result<Vec<usize>> {
            Ok(self
                .serving(want)?
                .into_iter()
                .filter(|&o| is_strand(o) && named(o))
                .collect())
        };
        let (want, _) = self.want_of(&asked_wanted);
        let mut strands = strands_of(&want)?;

        // A column the strand does not hold: its keys found along it, for what the conditions read
        // and the distance, the table then read kept to them.
        let keys_at: Vec<usize> = {
            let mut levels = Vec::new();
            for l in &table.orders[own].levels {
                if !levels.contains(&l.column()) {
                    levels.push(l.column());
                }
            }
            levels
        };
        let mut walked_columns = wanted.clone();
        let keyed = strands.is_empty();
        let along: Question;
        let walked_q = if keyed {
            let conditions = s
                .selection
                .as_ref()
                .map_or(relations::Named::of(""), |w| relations::Named::of(&w.to_string()));
            walked_columns = stored
                .iter()
                .copied()
                .filter(|&c| {
                    c == a.column
                        || keys_at.contains(&c)
                        || conditions.names(Some(&a.qualifier), &table.columns[c].name)
                })
                .collect();
            let Ok(q) = relations::parsed(&self.catalog, &select(&walked_columns), &from, &conds)
            else {
                return Ok(Nearest::Not);
            };
            along = q;
            &along
        } else {
            &asked_wanted
        };
        let (want, columns) = self.want_of(walked_q);
        if keyed {
            strands = strands_of(&want)?;
        }
        // Rows it may hold beyond its value's walk, kept to its keys, are read only by reached rows.
        if strands.is_empty() || (keyed && reached.is_none()) {
            return Ok(match order {
                Some(_) => Nearest::Not,
                None => Nearest::Table(self.touched() - before, self.pages_held() - held_before),
            });
        }
        let mut best: Option<(usize, Best)> = None;
        for o in strands {
            let b = self.best_first(o, &want, a.column, point)?;
            if best
                .as_ref()
                .is_none_or(|(_, x)| b.walk().top.1 < x.walk().top.1)
            {
                best = Some((o, b));
            }
        }
        let (oi, mut best) = best.expect("a strand serves");
        let table_rows = best.walk().table_rows;
        let need = reached.filter(|&n| n < table_rows);
        let tag = order_tag(a.table, oi);
        let fewest = match need {
            Some(n) => self.best_fewest(&mut best, &want, n)?,
            None => 0,
        };

        // Priced against the table's own read, every column it stores, and its sort.
        let mut consulted = Vec::new();
        if order.is_none() && !table.orders[oi].table_order {
            let every = relations::parsed(&self.catalog, &select(&stored), &from, &[])?;
            let all = Want {
                table,
                side: &every.sides[0],
                columns: stored.clone(),
                countable: None,
                keys: None,
            };
            let whole = self.start(own, &all)?;
            let held = self.reader.held_under(order_tag(a.table, own));
            let whole_file = whole.top.1.saturating_sub(held);
            let width: u64 = stored
                .iter()
                .map(|&c| held_width(table.columns[c].ty))
                .sum();
            let room = self.reader.room().sizes().bytes;
            let sort = 2 * (whole.table_rows * width)
                .saturating_sub(room)
                .div_ceil(PAGE as u64);
            let held = self.reader.held_under(tag);
            let along = match need {
                Some(n) => {
                    let read = fewest.saturating_sub(held);
                    match keyed {
                        true => {
                            let leaves: usize = wanted
                                .iter()
                                .map(|&c| table.orders[own].slots(table, c).map_or(0, |s| s.len()))
                                .sum();
                            read + whole.top.1.min(n.saturating_mul(leaves as u64))
                        }
                        false => read,
                    }
                }
                None if want.side.is_whole() => best.walk().top.1.saturating_sub(held),
                None => {
                    let part = self.part(&want, Some(&table.orders[oi].name))?;
                    part.plan.column_pages - part.plan.held
                }
            };
            consulted.push(Consulted {
                order: table.orders[own].name.clone(),
                directory_pages: whole.directory_pages,
                low: whole.top.0,
                high: whole.top.1,
                exact: None,
                held: 0,
            });
            if along >= whole_file + sort {
                return Ok(Nearest::Table(
                    self.touched() - before,
                    self.pages_held() - held_before,
                ));
            }
        }

        // Rows that come nearest first sort with nothing more where the distance is all it orders by.
        if !a.more && !keyed {
            stream.along(Led::Ordered);
        }
        self.mark_read(a.table, best.mark_whole())?;
        let (walk_rows, walk_taken) = (self.walked.get(), self.taken.get());
        let on_strand = Columns {
            at: &walked_columns
                .iter()
                .map(|c| stored.iter().position(|x| x == c).expect("a stored column"))
                .collect::<Vec<_>>(),
            width: stored.len(),
        };
        let key_places: Vec<usize> = keys_at
            .iter()
            .filter_map(|c| stored.iter().position(|x| x == c))
            .collect();
        let mut keys: Vec<Vec<Value>> = Vec::new();
        let hand = match keyed {
            true => Hand::Keys(&key_places, &mut keys),
            false => Hand::Stream(names.clone()),
        };
        let mut read = Walked {
            touched: self.touched() - before,
            touched_held: self.pages_held() - held_before,
            ..Default::default()
        };
        read.predicted = read.touched;
        self.read_nearest(
            &mut best,
            &want,
            &columns,
            &on_strand,
            walked_q,
            a.distance,
            &mut stream,
            need,
            strict,
            hand,
            &mut read,
        )?;
        let w = best.walk();
        let mut round = Round {
            order: table.orders[oi].name.clone(),
            served: vec![table.orders[oi].name.clone()],
            consulted,
            directory_pages: read.touched,
            directory_pages_held: read.touched_held,
            directory_pages_predicted: read.predicted,
            column_pages_top_low: w.top.0,
            column_pages_top_high: w.top.1,
            column_pages_low: if want.side.is_whole() { fewest } else { 0 },
            column_pages_high: w.top.1,
            column_pages_predicted: read.column_predicted,
            column_pages_held: read.column_held,
            column_pages_read: read.column_read,
            entries: w.statuses.0 + w.statuses.1 + w.statuses.2,
            entries_none: w.statuses.0,
            entries_may: w.statuses.1,
            entries_must: w.statuses.2,
            blocks_read: read.blocks,
            rows_held: read.rows_held as u64,
            rows_walked: self.walked.get() - walk_rows,
            rows_taken: self.taken.get() - walk_taken,
            ..Default::default()
        };
        round.consulted.push(Consulted {
            order: table.orders[oi].name.clone(),
            directory_pages: w.directory_pages,
            low: round.column_pages_low,
            high: w.top.1,
            exact: Some(read.column_predicted),
            held: read.column_held,
        });
        drop(best);
        if keyed && !keys.is_empty() {
            let keep = Keep::of_values(key_places.clone(), std::mem::take(&mut keys));
            let kept = Kept::of(&keep, 0, table);
            let most = std::cell::Cell::new(None);
            let on_table = Columns {
                at: &wanted
                    .iter()
                    .map(|c| stored.iter().position(|x| x == c).expect("a stored column"))
                    .collect::<Vec<_>>(),
                width: stored.len(),
            };
            let walk = round.clone();
            let r = self.read_blocks(
                &asked_wanted,
                None,
                kept.set.as_ref(),
                &most,
                &mut |rows| {
                    let rows: Vec<Vec<Value>> = rows
                        .into_iter()
                        .map(|r| on_table.row(r))
                        .filter(|row| kept.admits(row))
                        .collect();
                    stream.feed(Relation {
                        names: names.clone(),
                        rows,
                        order: None,
                    })
                },
            )?;
            let name = r.order.clone();
            round.add_table(&r, &name);
            round.rows_walked += r.rows_walked;
            round.rows_taken += r.rows_taken;
            round.reads = vec![walk, r];
        }
        round.rows_held = round.rows_held.max(stream.most_held() as u64);
        let out = stream.finish()?;
        let batch = pipeline::batch_of(&out)?;
        round.rows = batch.num_rows() as u64;
        Ok(Nearest::Answered(batch, round))
    }

    /// A strand's blocks nearest first: each entry's directory page, or each block's rows once its
    /// pages are known, while the rows asked for may yet lie nearer than the next entry's box.
    #[allow(clippy::too_many_arguments)]
    fn read_nearest(
        &self,
        best: &mut Best,
        want: &Want,
        columns: &[(usize, bool)],
        on_strand: &Columns,
        q: &Question,
        distance: &Expr,
        stream: &mut Stream,
        need: Option<u64>,
        strict: bool,
        mut hand: Hand,
        read: &mut Walked,
    ) -> Result<()> {
        if need == Some(0) {
            return Ok(());
        }
        let table = want.table;
        let mut frontier = Frontier::new();
        let (mut handed, mut last, mut unsorted) = (0u64, None::<Near>, false);
        loop {
            let next = best.next().unwrap_or(Near::PAST);
            let most = need.map(|n| n.saturating_sub(handed));
            let (rows, at) = frontier.release(next, strict, most, last);
            last = at;
            if !rows.is_empty() {
                handed += rows.len() as u64;
                unsorted = true;
                match &mut hand {
                    Hand::Stream(names) => stream.feed(Relation {
                        names: names.clone(),
                        rows,
                        order: None,
                    })?,
                    Hand::Keys(at, keys) => keys.extend(rows.into_iter().map(|row| {
                        at.iter().map(|&p| row[p].clone()).collect::<Vec<Value>>()
                    })),
                }
            }
            let ended = last.is_none_or(|g| frontier.nearest().min(next) > g);
            if ended && unsorted {
                stream.sort_so_far()?;
                unsorted = false;
            }
            if need.is_some_and(|n| handed >= n) && (ended || !strict) {
                break;
            }
            if best
                .next()
                .is_some_and(|n| frontier.past_least.is_none_or(|p| n <= p))
            {
                let (touched, held) = (self.touched(), self.pages_held());
                let plan = self.best_step(best, want)?;
                read.touched += self.touched() - touched;
                read.touched_held += self.pages_held() - held;
                read.predicted += self.touched() - touched;
                let Some(plan) = plan else {
                    continue;
                };
                read.predicted += plan.map_pages;
                read.column_predicted += plan.column_pages;
                read.blocks += plan.blocks_read();
                self.keep_held(&plan, want)?;
                let pages = self.reader.pages_read();
                let mut blocks = Blocks {
                    table,
                    q,
                    answer: Answer::new(&q.select).typed(&[table]),
                    most: None,
                    feed: |batch: RecordBatch| {
                        let rows = pipeline::relation_of(&batch)?
                            .rows
                            .into_iter()
                            .map(|r| on_strand.row(r))
                            .collect();
                        for (d, row) in stream.kept_with(rows, distance)? {
                            frontier.push(near_of(&d)?, row)?;
                        }
                        Ok(())
                    },
                };
                let done = self.scan_blocks(&plan, want, columns, &mut blocks, 0..1);
                self.reader.read_done();
                drop(blocks);
                done?;
                let maps = plan.maps_read();
                read.touched += maps.touched;
                read.touched_held += maps.held;
                read.column_read += self.reader.pages_read() - pages - maps.file;
                read.column_held += plan.held_read();
                best.read(plan);
                continue;
            }
            if frontier.past.is_some() {
                frontier.read_back()?;
                continue;
            }
            break;
        }
        read.rows_held = frontier.most;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expr(text: &str) -> Expr {
        sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
            .try_with_sql(text)
            .unwrap()
            .parse_expr()
            .unwrap()
    }

    #[test]
    fn a_value_is_fixed_where_it_reads_no_column_and_nothing_a_statement_draws() {
        for text in [
            "point '(1,2)'",
            "'(1,2)'::point",
            "(point '(1,2)')",
            "point(1, 2)",
            "point(1 + 1, -2)",
            "$1",
        ] {
            assert!(fixed(&expr(text)), "{text}");
        }
        for text in ["v", "t.v", "point(id, 0)", "point(random(), 0)", "clock_timestamp()"] {
            assert!(!fixed(&expr(text)), "{text}");
        }
    }
}
