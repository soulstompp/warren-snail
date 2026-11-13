//! A write's round. The rows a statement inserts, deletes or replaces are routed on the table's order
//! and on every index that holds their columns, each to the block its places name. A row deleted is
//! marked dead in its block's page map; a row inserted is written into each column's last page of its
//! block, the page written afresh; a block whose last page has no room left is rewritten in order,
//! its dead rows dropped, and cut again. Every page changed is written to a fresh page, the directory
//! up to its top, and the commit is the root.

use super::fill;
use super::foreign::Touched;
use super::statement::{self, Datum, Fill, Row, Source, Statement};
use super::{Snail, Wasted};
use crate::catalog::{assignable, Catalog, ColumnType, Level, Order, Table};
use crate::cut::ROW_GROUP_ROWS;
use crate::directory::{
    build_from, decode_page, decode_top, encode_page, encode_top, map_child, maps_held, pack,
    read_map, Entry, Head, MapPages, PageEntry, PageMap, MAP_HEAD,
};
use crate::encode::{decode_leaf, tail_pages};
use crate::file::{
    decode_free, decode_runs, encode_free, encode_runs, Extent, Place, Root, Run, Writer,
    REUSED_KEPT, STAGED,
};
use crate::lock;

/// What a transaction answers once one of its statements failed, until it ends, as PostgreSQL does.
pub(super) const ABORTED: &str =
    "current transaction is aborted, commands ignored until end of transaction block";
use crate::places::{byte_key, key, month_of, null_key, KeyValue};
use crate::values::Values;
use crate::write::{empty_head, head_of, laid_out, leaf_descriptors, write_block, Laid, Rows};
use crate::error::NOT_ASSIGNED;
use crate::{Error, Result};
use arrow::array::{
    Array, BinaryArray, BooleanArray, FixedSizeBinaryArray, Int16Array, Int32Array, Int64Array,
    RecordBatch, StringArray, TimestampMicrosecondArray,
};
use parquet::data_type::ByteArray;
use std::collections::BTreeMap;

/// What a statement did.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Done {
    pub statement: &'static str,
    /// The rows it inserted, updated or deleted.
    pub rows: u64,
    /// The generation the file stood at when it ended: a statement outside a transaction commits.
    pub generation: u64,
    pub pages_read: u64,
    /// The pages written, the root among them where the statement committed.
    pub pages_written: u64,
    /// The blocks rewritten in order.
    pub rewritten: u64,
    /// The blocks its commit pruned: those reads outside a transaction found wasted since the
    /// writer's last commit that wrote.
    pub blocks_pruned: u64,
    /// The orders it wrote: those holding a column it changed.
    pub orders: u64,
    /// Of the pages written, those written into pages freed before.
    pub pages_reused: u64,
    /// A question's answer.
    pub answer: Option<RecordBatch>,
    /// The bytes COPY … TO STDOUT wrote.
    pub copied: Option<Vec<u8>>,
    /// What it said beside its answer, each a line as PostgreSQL's client prints its WARNING or
    /// NOTICE.
    pub notices: Vec<String>,
}

/// How a writer takes a block's lock: waiting while another writer has it, as long as it takes,
/// unless the writers waiting make a ring; or at once or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Waiting {
    Wait,
    Try,
}

/// How a block's rows are written: its effects on it as it stands; the block rewritten in order; or
/// its effects on the version a transaction of one snapshot reads, which only that transaction reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Writing {
    Effects,
    Rewritten,
    Beside,
}

/// One row's change on a table.
#[derive(Clone)]
pub(super) enum Change {
    Insert(Row),
    Delete(Row),
    Replace(Row, Row),
}

/// One row's change on one order: its key there, the values the order holds, for a row replaced, the
/// key of the row that replaced it, and whether it lands a key in its block: a row inserted, or one
/// written by an UPDATE not in place.
pub(super) struct Effect {
    pub(super) key: Vec<KeyValue>,
    pub(super) values: Vec<Datum>,
    pub(super) delete: bool,
    pub(super) chain: Option<Vec<KeyValue>>,
    pub(super) lands: bool,
}

/// A block a transaction began from, by its first key and its page map then, and the entries that
/// stand for it now, their pages staged.
#[derive(Clone)]
pub(super) struct Rebased {
    first: Vec<KeyValue>,
    child: Extent,
    entries: Vec<Entry>,
    /// For an order that held no row, the head its first rows gave it.
    head: Option<Head>,
}

impl Rebased {
    /// The block's first key as it began from it.
    pub(super) fn first_key(&self) -> &[KeyValue] {
        &self.first
    }

    /// The entries that stand for it now.
    pub(super) fn staged_entries(&self) -> &[Entry] {
        &self.entries
    }
}

/// What a statement under way had staged when it began, the root it read with its writes in place,
/// what a transaction of one snapshot had changed (which its commit writes) and the tables changed
/// without being named, and what its foreign keys had recorded.
pub(super) struct Mark {
    root: Root,
    own: BTreeMap<String, super::beside::OwnRows>,
    first_bytes: BTreeMap<u64, u64>,
    unnamed: std::collections::BTreeSet<usize>,
    rebased: BTreeMap<(usize, usize), Vec<Rebased>>,
    staged: u64,
    touched: Touched,
    pending: Touched,
    modes: BTreeMap<(usize, usize), bool>,
}

/// The most times a statement's calls of the sequence functions are replayed before it is refused:
/// a run answered from its script makes the same calls unless another writer moved a sequence its
/// course turns on.
const REPLAYS: usize = 16;

/// What a statement that calls the sequence functions carries from one run to the next: what the
/// session held of them as it began, where each sequence stands, and the calls its replays made,
/// each with what it came to.
pub(super) struct Runs {
    start: super::sequences::Drawn,
    /// The values the session held when the statement began.
    held: std::collections::HashMap<String, std::collections::VecDeque<i64>>,
    stands: std::collections::HashMap<String, (i64, bool)>,
    script: Vec<(super::sequences::Op, super::sequences::Outcome)>,
    replays: usize,
}

/// The catalog a statement or a transaction declared: the catalog as committed it declared on, its
/// DDL now, its own statements, and the orders it made, each empty until rows come.
#[derive(Clone)]
pub(super) struct Declared {
    from: (Extent, u64),
    ddl: String,
    /// Its own statements, each as `fold::folded` left it, in their order: what it declares again
    /// on the catalog as committed, where another writer's commit moves it.
    own: String,
    made: Vec<(String, String)>,
    /// The sequences it made or restarted, which stand where the DDL leaves them.
    resets: Vec<String>,
    /// The orders a DROP of it took out, by table and name, which its commit lets go: of a table
    /// dropped, or an index a drop took with it.
    taken_out: Vec<(String, String)>,
    /// The relations it made, by name, which no other writer names until it commits: their locks
    /// are no other's to meet.
    relations: Vec<String>,
    /// The schemas it made, by name, as no other writer names them until it commits.
    schemas: Vec<String>,
    /// The types, domains and functions it made, as no other writer names them until it commits.
    objects: Vec<crate::oids::Object>,
    /// The OIDs each of its own statements took from the counter every writer of the file shares,
    /// which it keeps wherever it is declared again: in a transaction block, as each was declared.
    given: Vec<Took>,
}

/// The OIDs one of a transaction's own statements took from the file's counter: its place among
/// them, the first, and how many.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Took {
    pub(super) place: usize,
    pub(super) first: u32,
    pub(super) n: u32,
}

impl Declared {
    /// Whether it made the relation of that name.
    pub(super) fn made_relation(&self, name: &str) -> bool {
        self.relations.iter().any(|r| r == name)
    }

    /// Whether it made the schema of that name.
    pub(super) fn made_schema(&self, name: &str) -> bool {
        self.schemas.iter().any(|s| s == name)
    }

    /// Whether it made the type, domain or function.
    pub(super) fn made_object(&self, o: &crate::oids::Object) -> bool {
        self.objects.contains(o)
    }

    /// The catalog as committed it declared on.
    pub(super) fn from(&self) -> (Extent, u64) {
        self.from
    }

    /// Its own statements, in their order.
    pub(super) fn own(&self) -> &str {
        &self.own
    }

    /// The OIDs its own statements took from the file's counter.
    pub(super) fn given(&self) -> &[Took] {
        &self.given
    }

    /// The orders it made, by table and name.
    pub(super) fn made(&self) -> &[(String, String)] {
        &self.made
    }

    /// The orders a DROP of it took out, by table and name.
    pub(super) fn taken_out(&self) -> &[(String, String)] {
        &self.taken_out
    }
}

/// A directory page read on the way down, and the pages under it read too.
pub(super) struct Node {
    pub(super) entries: Vec<Entry>,
    pub(super) height: u32,
    pub(super) kids: BTreeMap<usize, Node>,
    /// The page it was read from; none at the top.
    pub(super) at: Extent,
}

/// A statement's first word, in capitals.
fn first_word(sql: &str) -> String {
    sql.trim_start()
        .split(|c: char| !c.is_ascii_alphabetic())
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase()
}

/// Whether a statement is a question by its first word, which its round answers.
fn asks(sql: &str) -> bool {
    matches!(first_word(sql).as_str(), "SELECT" | "VALUES" | "TABLE")
}

/// Whether a statement may name a relation, as a transaction's end, its savepoints and the
/// session's settings do not.
fn names_relations(sql: &str) -> bool {
    let first = first_word(sql);
    !matches!(
        first.as_str(),
        "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "ABORT"
            | "SAVEPOINT"
            | "RELEASE"
            | "SET"
            | "RESET"
            | "SHOW"
    )
}

/// The objects `after` holds that `before` does not whose rows take a key of the catalog's unique
/// indexes (a schema, a relation, a type or an array, a function), in the order PostgreSQL writes
/// their rows: as the DDL made them, a relation's row type and its array before its pg_class row.
pub(super) fn names_made(before: &Catalog, after: &Catalog) -> Vec<crate::oids::Object> {
    use crate::oids::Object as O;
    let made: Vec<&O> = after
        .oids
        .each()
        .map(|(_, o)| o)
        .filter(|o| matches!(o, O::Namespace(_) | O::Class(_) | O::Type(_) | O::Array(_) | O::Proc(..)))
        .filter(|o| before.oids.of(o).is_none())
        .collect();
    let mut out: Vec<O> = Vec::new();
    for o in &made {
        if let O::Class(n) = o {
            for t in [O::Type(n.clone()), O::Array(n.clone())] {
                if made.contains(&&t) && !out.contains(&t) {
                    out.push(t);
                }
            }
        }
        if !out.contains(o) {
            out.push((*o).clone());
        }
    }
    out
}

/// The entry, among an ordered page's, whose rows a key belongs with: the last whose first key is at
/// most the key, or the first.
pub(super) fn pick(entries: &[Entry], key: &[KeyValue]) -> usize {
    entries
        .partition_point(|e| e.first.as_slice() <= key)
        .saturating_sub(1)
}

fn datum_at(v: &Values, i: usize) -> Datum {
    match v {
        Values::Int(x) => x[i].map_or(Datum::Null, Datum::Int),
        Values::Wide(x) => x[i].map_or(Datum::Null, Datum::Wide),
        Values::Float4(x) => x[i].map_or(Datum::Null, Datum::Float4),
        Values::Float8(x) => x[i].map_or(Datum::Null, Datum::Float8),
        Values::Composed(x) => x[i].clone().map_or(Datum::Null, Datum::Compound),
        Values::Interval(x) => x[i].map_or(Datum::Null, Datum::Interval),
        _ => v
            .text(i)
            .map_or(Datum::Null, |s| Datum::Text(s.to_string())),
    }
}

/// A value of a column of type `ty`: bytea's bytes and a bit string's key as they are, any other as
/// `datum_at` reads it.
pub(super) fn datum_typed(v: &Values, i: usize, ty: ColumnType) -> Datum {
    match ty {
        ty if ty.held_as_bytes() => v
            .text_value(i)
            .map_or(Datum::Null, |b| Datum::Bytes(b.data().to_vec())),
        _ => datum_at(v, i),
    }
}

/// A value's key on a level of `column`, from the value: a number's, a string's or bytes'; a
/// string's under a library collation, by `collator`'s key.
pub(super) fn level_key(
    column: &crate::catalog::Column,
    collator: Option<&crate::collations::Collator>,
    level: Level,
    d: &Datum,
    null: KeyValue,
) -> KeyValue {
    let ty = column.ty;
    match d {
        // An infinite instant's month is NULL, as PostgreSQL's extract gives it.
        Datum::Int(v) => match level {
            Level::MonthUtc(_) if month_of(*v).is_none() => null,
            level => key(level, *v),
        },
        Datum::Interval(v) => KeyValue::Wide(v.total()),
        Datum::Text(t) => crate::places::column_byte_key(
            column,
            &crate::strings::column_key(ty, collator, t.as_bytes()),
        ),
        Datum::Bytes(b) => match crate::plane::on_plane(ty) {
            true => KeyValue::Wide(crate::plane::key_of_bytes(ty, b).unwrap_or(i128::MAX)),
            false => byte_key(ty, b),
        },
        Datum::Wide(k) => KeyValue::Wide(*k),
        Datum::Float4(b) => key(level, crate::scalars::float4_key(*b)),
        Datum::Float8(b) => key(level, crate::scalars::float8_key(*b)),
        // A composed value by its key, written as text that sorts as the key does.
        Datum::Compound(c) => KeyValue::Text(c.hex.clone()),
        Datum::Null => null,
    }
}

pub(super) fn push(v: &mut Values, d: &Datum) {
    match (v, d) {
        (Values::Int(x), Datum::Int(n)) => x.push(Some(*n)),
        (Values::Int(x), Datum::Null) => x.push(None),
        (Values::Text(x), Datum::Text(s)) => x.push(Some(s.clone())),
        (Values::Text(x), Datum::Null) => x.push(None),
        (Values::Utf8(x), Datum::Bytes(b)) => x.push(Some(ByteArray::from(b.clone()))),
        (Values::Utf8(x), Datum::Null) => x.push(None),
        (Values::Wide(x), Datum::Wide(k)) => x.push(Some(*k)),
        (Values::Wide(x), Datum::Null) => x.push(None),
        (Values::Float4(x), Datum::Float4(b)) => x.push(Some(*b)),
        (Values::Float4(x), Datum::Null) => x.push(None),
        (Values::Float8(x), Datum::Float8(b)) => x.push(Some(*b)),
        (Values::Float8(x), Datum::Null) => x.push(None),
        (Values::Composed(x), Datum::Compound(c)) => x.push(Some(c.clone())),
        (Values::Composed(x), Datum::Null) => x.push(None),
        (Values::Interval(x), Datum::Interval(i)) => x.push(Some(*i)),
        (Values::Interval(x), Datum::Null) => x.push(None),
        _ => panic!("a value pushed onto a column of another kind"),
    }
}

/// A value of a table's column as each of the leaves it is held as holds it: the value itself
/// where the column is one leaf.
fn leaf_datums(table: &Table, column: usize, d: &Datum) -> Vec<Datum> {
    let ty = table.columns[column].ty;
    if !table.is_group(column) {
        return vec![d.clone()];
    }
    // A composed value whole, for each of its leaves, each striped from it where it is written; a
    // path, a polygon or a snapshot too.
    if ty.tree().is_some() {
        return vec![d.clone(); ty.leaf_count()];
    }
    let mut v = Values::new(ty);
    push(&mut v, d);
    v.into_leaves(ty)
        .iter()
        .zip(ty.leaves())
        .map(|(l, leaf)| datum_typed(l, 0, leaf.ty))
        .collect()
}

/// The columns of a table's first unique order of plain NOT NULL columns, which know its rows;
/// none where it has no such order.
fn unique_key(table: &Table) -> Option<Vec<usize>> {
    table.orders.iter().filter(|o| o.unique).find_map(|o| {
        o.levels
            .iter()
            .map(|l| match l {
                Level::Column(c) if table.columns[*c].not_null => Some(*c),
                _ => None,
            })
            .collect::<Option<Vec<usize>>>()
    })
}

/// A row's key on an order of `table`.
pub(super) fn key_of(table: &Table, order: &Order, row: &Row) -> Result<Vec<KeyValue>> {
    order
        .levels
        .iter()
        .enumerate()
        .map(|(li, &l)| {
            let column = &table.columns[l.column()];
            Ok(level_key(
                column,
                column.orderer()?,
                l,
                &row[l.column()],
                null_key(order, li),
            ))
        })
        .collect()
}

/// The type each column a question returns takes before any row is read, as PostgreSQL types it: a
/// column's from its table, an expression's as the pipeline types it; none where it is not known.
pub(super) fn stated_types(catalog: &Catalog, question: &str) -> Vec<Option<ColumnType>> {
    use sqlparser::ast::{SelectItem, SetExpr, Statement as Sql, TableFactor};
    let Ok(parsed) = super::image::parse_sql(question) else {
        return Vec::new();
    };
    let [Sql::Query(q)] = parsed.as_slice() else {
        return Vec::new();
    };
    let SetExpr::Select(s) = q.body.as_ref() else {
        return Vec::new();
    };
    let mut columns: Vec<super::typed::TypedColumn> = Vec::new();
    for item in &s.from {
        for f in std::iter::once(&item.relation).chain(item.joins.iter().map(|j| &j.relation)) {
            let TableFactor::Table { name, alias, .. } = f else {
                continue;
            };
            let n = crate::catalog::object_name(name);
            let Some(t) = catalog.table(&n) else {
                continue;
            };
            let q = alias
                .as_ref()
                .map(|a| crate::catalog::ident(&a.name))
                .unwrap_or_else(|| n.rsplit('.').next().unwrap_or(&n).to_string());
            for c in &t.columns {
                columns.push((Some(q.clone()), c.name.clone(), Some(c.ty)));
            }
        }
    }
    let mut typing = super::typed::Typing {
        columns: &columns,
        release: catalog.release,
        types: Default::default(),
    };
    s.projection
        .iter()
        .map(|p| match p {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                typing.type_of(e).ok().flatten()
            }
            _ => None,
        })
        .collect()
}

/// The rows of an answer whose columns are the table's, in its order.
pub(super) fn rows_of(batch: &RecordBatch) -> Result<Vec<Row>> {
    let mut rows = vec![Vec::with_capacity(batch.num_columns()); batch.num_rows()];
    let schema = batch.schema();
    for (a, field) in batch.columns().iter().zip(schema.fields()) {
        let any = a.as_any();
        let scalar = crate::scalars::field_scalar(field);
        let bits = crate::strings::field_string(field).filter(|t| t.is_bits());
        // A geometric value, an aclitem or a snapshot, its leaves made into the value a row holds.
        let group = crate::strings::field_string(field).filter(|t| t.is_group() && !t.is_bits());
        let composed = crate::composed::field_composed(field);
        for (r, row) in rows.iter_mut().enumerate() {
            row.push(if a.is_null(r) {
                Datum::Null
            } else if let Some(ColumnType::Interval { .. }) = scalar {
                Datum::Interval(
                    crate::scalars::interval_at(a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not an interval", field.name()))
                    })?,
                )
            } else if let Some(ty) = bits {
                Datum::Bytes(crate::strings::bits_key_at(ty, a.as_ref(), r).unwrap())
            } else if let Some(ty) = group {
                crate::composed::group_at(ty, a.as_ref(), r)
                    .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?
            } else if let Some(ty) = composed {
                let c = crate::composed::compound_at(ty, a.as_ref(), r)
                    .ok_or_else(|| Error::new(format!("{} is not of its type", field.name())))?;
                match &c.item {
                    // An enum, as its rank.
                    crate::composed::Item::Scalar(d) => d.clone(),
                    _ => Datum::Compound(c),
                }
            } else if let Some(ty) = scalar {
                Datum::Int(
                    crate::scalars::scalar_key_at(ty, a.as_ref(), r).ok_or_else(|| {
                        Error::new(format!("{} is not of its type", field.name()))
                    })?,
                )
            } else if let Some(x) = any.downcast_ref::<BooleanArray>() {
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<Int16Array>() {
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<Int32Array>() {
                Datum::Int(x.value(r) as i64)
            } else if let Some(x) = any.downcast_ref::<Int64Array>() {
                Datum::Int(x.value(r))
            } else if let Some(x) = any.downcast_ref::<TimestampMicrosecondArray>() {
                Datum::Int(x.value(r))
            } else if let Some(x) = any.downcast_ref::<arrow::array::Float32Array>() {
                Datum::Float4(x.value(r).to_bits())
            } else if let Some(x) = any.downcast_ref::<arrow::array::Float64Array>() {
                Datum::Float8(x.value(r).to_bits())
            } else if let Some(x) = any.downcast_ref::<StringArray>() {
                Datum::Text(x.value(r).to_string())
            } else if let Some((m, p, _)) = crate::numeric::decimal_at(a.as_ref(), r) {
                // A numeric column's answer is its smallest units, at the column's own scale, NaN
                // at its key: in 64 bits up to 18 digits, past them in 128.
                let units = m.unwrap_or_else(|| crate::numeric::nan_units(p));
                if p > 18 {
                    Datum::Wide(units)
                } else {
                    Datum::Int(units as i64)
                }
            } else if let Some(x) = any.downcast_ref::<BinaryArray>() {
                Datum::Bytes(x.value(r).to_vec())
            } else if let Some(x) = any
                .downcast_ref::<FixedSizeBinaryArray>()
                .filter(|x| x.value_length() == 16)
            {
                Datum::Wide(crate::scalars::uuid_key(x.value(r).try_into().unwrap()))
            } else {
                return Err(Error::new(format!(
                    "an answer of {} is not read back as a row",
                    a.data_type()
                )));
            });
        }
    }
    Ok(rows)
}

/// The type of an answer's column, as the snail answers a column of that type.
fn answer_type(dt: &arrow::datatypes::DataType) -> Result<ColumnType> {
    use arrow::datatypes::DataType as A;
    Ok(match dt {
        A::Int16 => ColumnType::Int2,
        A::Int32 => ColumnType::Int4,
        A::Int64 => ColumnType::Int8,
        A::Timestamp(..) => ColumnType::Timestamptz,
        A::Utf8 | A::LargeUtf8 | A::Utf8View => ColumnType::Text(None),
        A::Binary | A::LargeBinary | A::BinaryView => ColumnType::Bytes,
        A::Boolean => ColumnType::Boolean,
        A::FixedSizeBinary(16) => ColumnType::Uuid,
        A::Float32 => ColumnType::Float4,
        A::Float64 => ColumnType::Float8,
        A::Decimal128(..) | A::Struct(_) if crate::numeric::decimal_type(dt).is_some() => {
            let (p, s) = crate::numeric::decimal_type(dt).unwrap();
            ColumnType::numeric(Some((u32::from(p), i32::from(s))))
        }
        other => {
            return Err(Error::new(format!(
                "an answer of {other} is not read back as a row"
            )))
        }
    })
}

/// The relation a finder reads, as written, and the name it reads the table by: its alias, or the
/// last part of its name.
fn finder_relation(find: &str) -> Result<(String, String)> {
    use sqlparser::ast::{ObjectNamePart, SetExpr, Statement as Sql, TableFactor};
    let parsed = super::image::parse_sql(find)?;
    if let [Sql::Query(q)] = parsed.as_slice() {
        if let SetExpr::Select(s) = q.body.as_ref() {
            if let Some(TableFactor::Table { name, alias, .. }) =
                s.from.first().map(|f| &f.relation)
            {
                let by = match alias {
                    Some(a) => crate::catalog::ident(&a.name),
                    None => match name.0.last() {
                        Some(ObjectNamePart::Identifier(i)) => crate::catalog::ident(i),
                        _ => crate::catalog::object_name(name),
                    },
                };
                return Ok((s.from[0].relation.to_string(), by));
            }
        }
    }
    Err(Error::new("an UPDATE names a table"))
}

/// The type of each column of a question's answer, as the snail answers a column of that type;
/// refused where one that `fills` names, each a column of table `ti` and the answer's column that
/// fills it, is not assigned to its column by PostgreSQL's assignment casts. They are checked
/// before any row is read, as PostgreSQL checks them: a column of the answer holding no value,
/// every column of an answer of no rows, is given no type by its values, and takes the type its
/// question gives it, or where none is known fills only NULLs. A question whose columns its own
/// reading types, `typed`, gives each its answer's type.
fn assigned_types(
    catalog: &Catalog,
    question: &str,
    batch: &RecordBatch,
    ti: usize,
    fills: &[(usize, usize)],
    typed: bool,
) -> Result<Vec<ColumnType>> {
    let types = answer_types(batch)?;
    let stated = stated_types(catalog, question);
    let t = &catalog.tables[ti];
    for &(c, i) in fills {
        let col = &t.columns[c];
        let typed = typed || batch.column(i).null_count() < batch.num_rows();
        let from = if typed {
            Some(types[i])
        } else {
            stated.get(i).copied().flatten()
        };
        if let Some(from) = from.filter(|&from| !assignable(from, col.ty)) {
            return Err(not_assigned(col, from));
        }
    }
    Ok(types)
}

/// PostgreSQL's refusal of an expression of type `from` given to a column no assignment cast
/// reaches.
fn not_assigned(col: &crate::catalog::Column, from: ColumnType) -> Error {
    Error::new(format!(
        "column \"{}\" is of type {} but expression is of type {}{NOT_ASSIGNED}",
        col.name,
        super::typed::type_name(col.ty),
        super::typed::type_name(from)
    ))
}

/// Each column a row's question answers, with its place in the answer.
fn answered(fill: &[Fill]) -> Vec<(usize, usize)> {
    fill.iter()
        .enumerate()
        .filter_map(|(c, f)| match f {
            Fill::Answer(i) => Some((c, *i)),
            _ => None,
        })
        .collect()
}

/// A value given to a generated column, or to an identity GENERATED ALWAYS, refused as PostgreSQL
/// refuses it; its DEFAULT, drawn by the row's question or not, taken.
fn given_refused<'a>(t: &Table, rows: impl Iterator<Item = &'a Vec<Fill>>) -> Result<()> {
    for r in rows {
        for (col, f) in t.columns.iter().zip(r) {
            let always = col.identity.as_ref().is_some_and(|i| i.always);
            if (col.generated.is_some() || always) && !matches!(f, Fill::Default | Fill::Drawn(_))
            {
                let why = match col.generated.is_some() {
                    true => "a generated column",
                    false => "an identity column defined as GENERATED ALWAYS",
                };
                return Err(Error::new(format!(
                    "cannot insert a non-DEFAULT value into column \"{0}\": Column \"{0}\" is \
                     {why}.",
                    col.name
                )));
            }
        }
    }
    Ok(())
}

/// How many rows take each column's DEFAULT drawn for the statement, none drawn by a question.
fn defaults_taken<'a>(
    t: &Table,
    rows: impl Iterator<Item = &'a Vec<Fill>>,
) -> std::vec::IntoIter<(usize, usize)> {
    let mut counts = vec![0usize; t.columns.len()];
    for r in rows {
        for (c, f) in r.iter().enumerate() {
            if matches!(f, Fill::Default) {
                counts[c] += 1;
            }
        }
    }
    counts.into_iter().enumerate().collect::<Vec<_>>().into_iter()
}

/// Whether an INSERT's VALUES draws a DEFAULT inside its question, which only a statement run on
/// a tab answers.
fn drawn_in_its_question(st: &Statement) -> bool {
    match st.written() {
        Statement::Insert {
            source: Source::Computed(rows),
            ..
        } => rows
            .iter()
            .any(|(r, _)| r.iter().any(|f| matches!(f, Fill::Drawn(_)))),
        _ => false,
    }
}

/// The type of each column of an answer, as the snail answers a column of that type.
pub(super) fn answer_types(batch: &RecordBatch) -> Result<Vec<ColumnType>> {
    let types: Vec<ColumnType> = batch
        .schema()
        .fields()
        .iter()
        .map(|f| {
            match crate::composed::field_composed(f)
                .or_else(|| crate::scalars::field_scalar(f))
                .or_else(|| crate::strings::field_string(f))
            {
                Some(ty) => Ok(ty),
                None if crate::numeric::is_numeric_field(f)
                    && matches!(f.data_type(), arrow::datatypes::DataType::Utf8) =>
                {
                    Ok(ColumnType::NumericText(None))
                }
                None => answer_type(f.data_type()),
            }
        })
        .collect::<Result<_>>()?;
    Ok(types)
}

/// Rows of a table's stored columns widened to all its columns, a virtual one NULL.
pub(super) fn widen(table: &Table, rows: Vec<Row>) -> Vec<Row> {
    if !table.columns.iter().any(|c| c.is_virtual()) {
        return rows;
    }
    rows.into_iter()
        .map(|r| {
            let mut given = r.into_iter();
            table
                .columns
                .iter()
                .map(|c| {
                    if c.is_virtual() {
                        Datum::Null
                    } else {
                        given.next().unwrap_or(Datum::Null)
                    }
                })
                .collect()
        })
        .collect()
}

/// Whether a VALUES list's questions read relations through the file's volatile functions alone,
/// and through one at least (`own::sees_its_write`).
fn sees_its_write(catalog: &Catalog, rows: &[(Vec<Fill>, Option<String>)]) -> bool {
    let parsed: Vec<Vec<sqlparser::ast::Statement>> = rows
        .iter()
        .filter_map(|(_, q)| q.as_deref())
        .map(super::image::parse_sql)
        .collect::<Result<_>>()
        .unwrap_or_default();
    super::own::sees_its_write(catalog, &parsed)
}

/// The rows a statement found, each value in the form its table holds it: an answer gives a numeric
/// at the precision the path that read it computes, which need not be its column's own.
pub(super) fn found_rows(table: &Table, batch: &RecordBatch) -> Result<Vec<Row>> {
    let mut rows = widen(table, rows_of(batch)?);
    let stored = (0..table.columns.len()).filter(|&c| !table.columns[c].is_virtual());
    for (field, c) in batch.schema().fields().iter().zip(stored) {
        let to = table.columns[c].ty;
        if !to.is_numeric() {
            continue;
        }
        let from = match crate::numeric::is_numeric_field(field)
            && matches!(field.data_type(), arrow::datatypes::DataType::Utf8)
        {
            true => ColumnType::NumericText(None),
            false => answer_type(field.data_type())?,
        };
        for row in &mut rows {
            if row[c] == Datum::Null {
                continue;
            }
            let n = fill::numeric_of(&row[c], from).ok_or_else(|| {
                Error::new(format!("{}.{}: a row found holds no numeric", table.name, field.name()))
            })?;
            row[c] = fill::numeric_datum(n, to)?;
        }
    }
    Ok(rows)
}

/// The columns of a write's finder's answer its table stores, the values a join asks and the row's
/// ctid after them left out.
fn stored_of(table: &Table, batch: &RecordBatch) -> Result<RecordBatch> {
    let stored = table.columns.iter().filter(|c| !c.is_virtual()).count();
    Ok(batch.project(&(0..stored).collect::<Vec<_>>())?)
}

/// The rows of a write's finder's answer the write changes, by their places in it: where its finder
/// joins, the answer's last column is each row's ctid, and a row the join meets more than once is
/// changed once, at the first place it is met; where it does not, every row, rows alike each their
/// own.
fn met_once(table: &Table, batch: &RecordBatch) -> Result<Vec<usize>> {
    let stored = table.columns.iter().filter(|c| !c.is_virtual()).count();
    let n = batch.num_columns();
    if n <= stored {
        return Ok((0..batch.num_rows()).collect());
    }
    let ctids = batch.column(n - 1);
    let mut seen = std::collections::HashSet::new();
    let mut met = Vec::new();
    for i in 0..batch.num_rows() {
        let ctid = arrow::util::display::array_value_to_string(ctids.as_ref(), i)?;
        if seen.insert(ctid) {
            met.push(i);
        }
    }
    Ok(met)
}

/// What marks the fields a write's rows hold `old`'s and `new`'s columns in: names no statement can
/// write.
const KEPT: char = '\u{5}';

/// What names each of RETURNING's sequence calls an INSERT's question makes for a row, after the
/// row's own items.
pub(super) const RETURNED_CALL: &str = "\u{5}call";

/// How many of RETURNING's sequence calls a question made for each row: the fields at its end
/// named for them.
fn calls_made(batch: &RecordBatch) -> usize {
    batch
        .schema()
        .fields()
        .iter()
        .rev()
        .take_while(|f| f.name() == RETURNED_CALL)
        .count()
}

/// The field a write's row holds column `c` in: its own name for the row as the write left it,
/// marked for `old` and `new`.
fn kept_name(side: Option<&str>, c: &str) -> String {
    match side {
        None => c.to_string(),
        Some(s) => format!("{KEPT}{s}{KEPT}{c}"),
    }
}

/// A name quoted as SQL writes one.
fn quoted(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

/// A RETURNING list as a select list over a write's rows read by `alias`: `old.c` and `new.c` read
/// from their fields, `*` and `alias.*` the table's columns, `old.*` and `new.*` theirs, each item
/// named as PostgreSQL names it.
fn returning_list(
    t: &Table,
    alias: &str,
    items: &[sqlparser::ast::SelectItem],
) -> Result<String> {
    use sqlparser::ast::{Expr, Ident, SelectItem, SelectItemQualifiedWildcardKind as Kind};
    let ident = crate::catalog::ident;
    // `old` and `new` name the rows before and after, where the table is not read by either name.
    let side_of = |name: &Ident| -> Option<&'static str> {
        match ident(name).as_str() {
            n if n == alias => None,
            "old" => Some("old"),
            "new" => Some("new"),
            _ => None,
        }
    };
    let spread = |side: Option<&str>| -> Vec<String> {
        t.own_columns()
            .iter()
            .map(|c| format!("{} AS {}", quoted(&kept_name(side, &c.name)), quoted(&c.name)))
            .collect()
    };
    let rewritten = |e: &Expr| -> String {
        let mut e = e.clone();
        let _ = sqlparser::ast::visit_expressions_mut(&mut e, |x| {
            if let Expr::CompoundIdentifier(parts) = x {
                if let [q, c] = parts.as_slice() {
                    if let Some(side) = side_of(q) {
                        *x = Expr::Identifier(Ident::with_quote('"', kept_name(Some(side), &ident(c))));
                    }
                }
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        e.to_string()
    };
    let mut out = Vec::new();
    for item in items {
        match item {
            SelectItem::Wildcard(_) => out.extend(spread(None)),
            SelectItem::QualifiedWildcard(Kind::ObjectName(n), _) => {
                let q = crate::catalog::object_name(n);
                match n.0.as_slice() {
                    [p] if p.as_ident().and_then(side_of).is_some() => {
                        out.extend(spread(p.as_ident().and_then(side_of)))
                    }
                    _ if q == alias || q.rsplit('.').next() == Some(alias) => {
                        out.extend(spread(None))
                    }
                    _ => {
                        return Err(Error::new(format!(
                            "missing FROM-clause entry for table \"{}\"",
                            q.rsplit('.').next().unwrap_or(&q)
                        )))
                    }
                }
            }
            SelectItem::UnnamedExpr(e) => out.push(format!(
                "{} AS {}",
                rewritten(e),
                quoted(&super::pipeline::default_name(e))
            )),
            SelectItem::ExprWithAlias { expr, alias: a } => {
                out.push(format!("{} AS {}", rewritten(expr), quoted(&ident(a))))
            }
            other => return Err(Error::new(format!("RETURNING {other} is not read"))),
        }
    }
    Ok(out.join(", "))
}

/// A RETURNING list whose sequence calls each read the value its row's question made, each item
/// named as it is written.
fn called_as_made(items: &[sqlparser::ast::SelectItem]) -> Vec<sqlparser::ast::SelectItem> {
    use sqlparser::ast::{Expr, Ident, SelectItem};
    let calls: Vec<String> = super::statement::returned_calls(items)
        .iter()
        .map(ToString::to_string)
        .collect();
    let mut j = 0;
    let mut read = |e: &Expr| {
        let mut e = e.clone();
        let _ = sqlparser::ast::visit_expressions_mut(&mut e, |x| {
            if calls.get(j).is_some_and(|c| *c == x.to_string()) {
                *x = Expr::Identifier(Ident::with_quote('"', format!("{RETURNED_CALL}{j}")));
                j += 1;
            }
            std::ops::ControlFlow::<()>::Continue(())
        });
        e
    };
    items
        .iter()
        .map(|i| match i {
            SelectItem::UnnamedExpr(e) => SelectItem::ExprWithAlias {
                expr: read(e),
                alias: Ident::with_quote('"', super::pipeline::default_name(e)),
            },
            SelectItem::ExprWithAlias { expr, alias } => SelectItem::ExprWithAlias {
                expr: read(expr),
                alias: alias.clone(),
            },
            other => other.clone(),
        })
        .collect()
}

/// The sequence a column's DEFAULT draws from: its identity's, or its DEFAULT's nextval.
pub(super) fn sequence_of(catalog: &Catalog, col: &crate::catalog::Column) -> Option<String> {
    if let Some(i) = &col.identity {
        return Some(i.sequence.clone());
    }
    let mut e = col.default.as_ref()?;
    while let sqlparser::ast::Expr::Nested(x) | sqlparser::ast::Expr::Cast { expr: x, .. } = e {
        e = x;
    }
    let sqlparser::ast::Expr::Function(f) = e else {
        return None;
    };
    let name = crate::catalog::object_name(&f.name).to_lowercase();
    if name != "nextval" && name != "pg_catalog.nextval" {
        return None;
    }
    let sqlparser::ast::FunctionArguments::List(l) = &f.args else {
        return None;
    };
    let mut a = match l.args.first()? {
        sqlparser::ast::FunctionArg::Unnamed(sqlparser::ast::FunctionArgExpr::Expr(a)) => a,
        _ => return None,
    };
    while let sqlparser::ast::Expr::Nested(x) | sqlparser::ast::Expr::Cast { expr: x, .. } = a {
        a = x;
    }
    let sqlparser::ast::Expr::Value(v) = a else {
        return None;
    };
    let sqlparser::ast::Value::SingleQuotedString(text) = &v.value else {
        return None;
    };
    crate::ddl::regclass(text)
        .ok()
        .filter(|n| catalog.sequence(n).is_some())
}

/// The clock a DEFAULT reads, where it reads one: now, current_timestamp, transaction_timestamp,
/// statement_timestamp or clock_timestamp.
fn clock_of(e: &sqlparser::ast::Expr) -> Option<String> {
    let mut e = e;
    while let sqlparser::ast::Expr::Nested(x) | sqlparser::ast::Expr::Cast { expr: x, .. } = e {
        e = x;
    }
    let sqlparser::ast::Expr::Function(f) = e else {
        return None;
    };
    let name = crate::catalog::object_name(&f.name).to_lowercase();
    let name = name
        .strip_prefix("pg_catalog.")
        .unwrap_or(&name)
        .to_string();
    matches!(
        name.as_str(),
        "now"
            | "current_timestamp"
            | "transaction_timestamp"
            | "statement_timestamp"
            | "clock_timestamp"
    )
    .then_some(name)
}

/// A uuid a DEFAULT hands out, where it hands one out: gen_random_uuid() and uuidv4() a version 4,
/// uuidv7() a version 7.
fn uuid_of(e: &sqlparser::ast::Expr) -> Option<Result<i128>> {
    let mut e = e;
    while let sqlparser::ast::Expr::Nested(x) | sqlparser::ast::Expr::Cast { expr: x, .. } = e {
        e = x;
    }
    let sqlparser::ast::Expr::Function(f) = e else {
        return None;
    };
    let name = crate::catalog::object_name(&f.name).to_lowercase();
    match name.strip_prefix("pg_catalog.").unwrap_or(&name) {
        "gen_random_uuid" | "uuidv4" => Some(crate::scalars::uuid_v4()),
        "uuidv7" => Some(crate::scalars::uuid_v7()),
        _ => None,
    }
}

/// The machine's clock now, in microseconds since 1970 UTC.
pub(super) fn wall_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}
/// Refuses a row a table cannot hold.
fn check(table: &Table, row: &Row) -> Result<()> {
    if row.len() != table.columns.len() {
        return Err(Error::new(format!(
            "{}: a row of {} values for {} columns",
            table.name,
            row.len(),
            table.columns.len()
        )));
    }
    for (col, d) in table.columns.iter().zip(row) {
        let what = |why: String| Error::new(format!("{}.{}: {why}", table.name, col.name));
        match (col.ty, d) {
            (_, Datum::Null) => {}
            (ColumnType::Int2, Datum::Int(v)) if i16::try_from(*v).is_err() => {
                return Err(what(format!("{v} is out of range")))
            }
            (ColumnType::Int4, Datum::Int(v)) if i32::try_from(*v).is_err() => {
                return Err(what(format!("{v} is out of range")))
            }
            (ColumnType::Text(Some(n)), Datum::Text(s)) if s.chars().count() > n as usize => {
                return Err(what(format!(
                    "is varchar({n}) and the row holds {} characters",
                    s.chars().count()
                )))
            }
            (ColumnType::Bpchar(Some(n)), Datum::Text(s)) if s.chars().count() != n as usize => {
                return Err(what(format!(
                    "is character({n}) and the row holds {} characters",
                    s.chars().count()
                )))
            }
            (ColumnType::Name, Datum::Text(s)) if s.len() > crate::strings::NAME_BYTES => {
                return Err(what(format!("is name and the row holds {} bytes", s.len())))
            }
            (ColumnType::Bit(n), Datum::Bytes(k)) if crate::strings::bits_of_key(k).0 != n => {
                return Err(what(format!(
                    "is bit({n}) and the row holds {} bits",
                    crate::strings::bits_of_key(k).0
                )))
            }
            (ColumnType::Varbit(Some(n)), Datum::Bytes(k))
                if crate::strings::bits_of_key(k).0 > n =>
            {
                return Err(what(format!(
                    "is bit varying({n}) and the row holds {} bits",
                    crate::strings::bits_of_key(k).0
                )))
            }
            (ColumnType::Numeric { precision, .. }, Datum::Int(_) | Datum::Wide(_))
                if d.wide().is_some_and(|u| {
                    u != crate::numeric::nan_units(precision)
                        && u.unsigned_abs() >= 10u128.pow(precision as u32)
                }) =>
            {
                return Err(what(format!("{d:?} has more digits than it holds")))
            }
            (ColumnType::Numeric { precision, .. }, Datum::Wide(_)) if precision > 18 => {}
            (ColumnType::NumericText(_), Datum::Text(s))
                if crate::numeric::Numeric::parse(s).is_err() =>
            {
                return Err(what(format!("'{s}' is not a numeric")))
            }
            (ColumnType::Text(_) | ColumnType::Bpchar(_) | ColumnType::Name, Datum::Text(_))
            | (ColumnType::NumericText(_), Datum::Text(_))
            | (ColumnType::Textual(_), Datum::Text(_))
            | (ColumnType::Geometric(_), Datum::Bytes(_))
            | (ColumnType::Composed(_), Datum::Compound(_))
            | (ColumnType::Bytes | ColumnType::Bit(_) | ColumnType::Varbit(_), Datum::Bytes(_)) => {
            }
            (ColumnType::Internal(t), Datum::Text(_) | Datum::Bytes(_))
                if t.copied() && t.is_bytes() == matches!(d, Datum::Bytes(_)) => {}
            (ColumnType::Uuid, Datum::Wide(_))
            | (ColumnType::Float4, Datum::Float4(_))
            | (ColumnType::Float8, Datum::Float8(_))
            | (ColumnType::Interval { .. }, Datum::Interval(_)) => {}
            (
                ColumnType::Text(_)
                | ColumnType::Bytes
                | ColumnType::Bpchar(_)
                | ColumnType::Name
                | ColumnType::Bit(_)
                | ColumnType::Varbit(_)
                | ColumnType::Uuid
                | ColumnType::Float4
                | ColumnType::Float8
                | ColumnType::Interval { .. },
                Datum::Int(_),
            )
            | (_, Datum::Text(_))
            | (_, Datum::Bytes(_))
            | (_, Datum::Wide(_))
            | (_, Datum::Float4(_))
            | (_, Datum::Float8(_))
            | (_, Datum::Compound(_))
            | (_, Datum::Interval(_)) => {
                return Err(what(format!("{d:?} is not a value of {}", col.ty.name())))
            }
            (_, Datum::Int(_)) => {}
        }
    }
    Ok(())
}

impl Snail {
    /// Runs one statement that writes, or begins or ends a transaction. Outside a transaction a
    /// statement commits; inside one, its pages wait for the COMMIT, and its rows are what the
    /// transaction's later statements read.
    pub fn execute(&mut self, sql: &str) -> Result<Done> {
        self.execute_with(sql, &[])
    }

    /// Runs one statement with its parameters, `$1` and on, given as text, each read as a quoted
    /// literal is, by its context. A question among the statements is answered as `ask_with`
    /// answers it, its answer in what it did. What PostgreSQL warns of a time type's precision
    /// past 6 as it reads the statement comes before what it says as it runs it.
    pub fn execute_with(&mut self, sql: &str, params: &[String]) -> Result<Done> {
        let mut done = self.executed_with(sql, params)?;
        let warned = super::spelled::precisions_reduced(&self.catalog.functions, sql);
        done.notices.splice(0..0, warned);
        Ok(done)
    }

    /// A statement run as `execute_with` runs it, without what PostgreSQL warns as it reads it: a
    /// prepared statement's, read where it was prepared.
    pub(super) fn executed_with(&mut self, sql: &str, params: &[String]) -> Result<Done> {
        if self.writer.is_none() {
            return Err(Error::new(
                "this snail was opened to read; open it to write",
            ));
        }
        // The clock now() reads is the transaction's, begun at BEGIN or with a statement alone, and
        // so is a literal's `now` from the statement's first word.
        let at = wall_micros();
        self.clock.1 = at;
        if self.begun.is_none() {
            self.clock.0 = at;
        }
        // In a transaction of one snapshot a statement names as committed, before anything reads
        // its names; a question does as its round begins.
        if !self.aborted && names_relations(sql) && !asks(sql) {
            if let Err(e) = self.named_as_committed() {
                self.failed()?;
                return Err(e);
            }
            // At READ COMMITTED it names as committed when its round renews, which is before it is
            // read where another's commit moved the catalog.
            if let Err(e) = self.renewed_where_declared_since() {
                if self.begun.is_some() {
                    self.failed()?;
                }
                return Err(e);
            }
        }
        let _handed = super::handed::holding(&self.functions);
        let _own = super::own::holding(&self.catalog.functions, self.catalog.release);
        let _clocked = crate::scalars::clocked(self.clock.0, self.clock.1);
        let _zoned = self.in_zone();
        let _named = crate::oids::named(self.catalog.oids.clone());
        let _deparsing = crate::deparse::holding(&self.catalog, sql);
        let _standing =
            super::sequences::standing(&self.catalog, sql, || self.sequences_newest())?;
        let _drawing = super::prng::lending(&self.drawing);
        let _timing = self.timing();
        let written_out;
        let text = match params.is_empty() {
            true => sql,
            false => {
                written_out = statement::with_params(sql, params)?;
                written_out.as_str()
            }
        };
        // A statement refused fails its transaction, as one that fails as it runs does.
        if !self.aborted {
            if let Err(e) = self.parameters_read(sql, params) {
                if self.begun.is_some() {
                    self.failed()?;
                }
                return Err(e);
            }
        }
        // WHERE CURRENT OF a cursor finds the row it stands on by the table's key.
        let current;
        let text = match super::cursor::mentions_current_of(text) {
            true => match self.current_of(text) {
                Ok(t) => {
                    current = t;
                    current.as_str()
                }
                Err(e) => {
                    if self.begun.is_some() && !self.aborted {
                        self.failed()?;
                    }
                    return Err(e);
                }
            },
            false => text,
        };
        let st = match statement::parse(&self.catalog, text) {
            Ok(st) => st,
            // In a failed transaction only a statement's grammar is read before it is refused,
            // as PostgreSQL reads no name of it.
            Err(e) if self.aborted && e.sqlstate() != Some("42601") => {
                return Err(Error::new(ABORTED))
            }
            Err(e) => {
                if self.begun.is_some() && !self.aborted {
                    self.failed()?;
                }
                return Err(e);
            }
        };
        // A transaction one of whose statements failed takes no statement but its end, which rolls
        // it back, or a ROLLBACK TO one of its savepoints, as PostgreSQL does.
        if self.aborted {
            match st {
                Statement::Commit | Statement::Rollback => {
                    if let Some((began, _)) = self.begun.take() {
                        self.root = began;
                    }
                    self.abandon()?;
                    self.aborted = false;
                    self.ended(false);
                    return Ok(Done {
                        statement: "ROLLBACK",
                        generation: self.root.generation,
                        ..Default::default()
                    });
                }
                Statement::Session(
                    super::session::Said::RollbackTo { .. } | super::session::Said::Chain { .. },
                ) => {}
                _ => return Err(Error::new(ABORTED)),
            }
        }
        if let Statement::Question { .. } = st {
            return self.answered(sql, params);
        }
        if let Statement::Session(said) = st {
            return self.said(said, sql);
        }
        // A transaction READ ONLY takes no write, in PostgreSQL's words.
        let begun = self.begun.is_some();
        if self.levels.read_only(begun) {
            let what = match st.written() {
                Statement::Writes { writes, .. } => writes.first().map(|(_, w)| w.kind()),
                Statement::Insert { .. } => Some("INSERT"),
                Statement::Update { .. } | Statement::Set { .. } => Some("UPDATE"),
                Statement::Delete { .. } => Some("DELETE"),
                Statement::Declare { kind, .. } => Some(*kind),
                _ => None,
            };
            if let Some(what) = what {
                if begun {
                    self.failed()?;
                }
                return Err(Error::new(format!(
                    "cannot execute {what} in a read-only transaction"
                )));
            }
        }
        let writes = !matches!(
            st,
            Statement::Begin
                | Statement::Commit
                | Statement::Rollback
                | Statement::Isolation { .. }
        );
        let snapshot = writes
            && !matches!(
                st,
                Statement::Setting { .. } | Statement::Constraints { .. }
            );
        // Each statement reads what was committed when it began; in a transaction, with the
        // transaction's own changes in place (PostgreSQL's READ COMMITTED). At SERIALIZABLE the first
        // that reads takes the transaction's snapshot, its marks with it.
        let read_on = self.root.catalog_named();
        if snapshot && self.levels.serializable(begun) {
            if let Err(e) = self.serial_snapshot(begun) {
                self.failed()?;
                return Err(e);
            }
            self.held = begun;
        } else if self.begun.is_none() && self.rounds > 0 {
            self.renew()?;
        } else if self.begun.is_some() && writes && !self.held {
            self.renew_in_transaction()?;
            // At REPEATABLE READ, the generation the first read takes is the transaction's.
            self.held = self.levels.repeatable(true)
                && !matches!(
                    st,
                    Statement::Setting { .. } | Statement::Constraints { .. }
                );
        }
        // Where another's DDL committed after it was read and before its round renewed, it is read
        // again on the catalog as committed.
        let st = match self.root.catalog_named() == read_on {
            true => st,
            false => match statement::parse(&self.catalog, text) {
                Ok(again) => again,
                Err(e) => {
                    self.failed()?;
                    return Err(e);
                }
            },
        };
        self.rounds += 1;
        let (read0, written0, reused0) = (self.reader.pages_read(), self.written(), self.reused);
        let mut done = Done {
            statement: st.kind(),
            ..Default::default()
        };
        let _prepared = super::prepared::viewing(&self.session.prepared);
        let viewing = super::settings::viewing(self.settings_view());
        let changed = match st {
            Statement::Setting { .. } => false,
            Statement::Begin
            | Statement::Isolation {
                at: statement::Isolated::Begin,
                ..
            } => {
                let modes = match st {
                    Statement::Isolation { modes, .. } => modes,
                    _ => Default::default(),
                };
                // BEGIN in a transaction warns and goes on, its modes set for the transaction, as
                // PostgreSQL's does.
                if self.begun.is_some() {
                    done.notices
                        .push("WARNING:  there is already a transaction in progress".into());
                    if modes != Default::default() {
                        let sub = !self.savepoints.is_empty();
                        if let Err(e) =
                            self.levels
                                .set(modes, statement::Isolated::Transaction, true, sub)
                        {
                            self.failed()?;
                            return Err(e);
                        }
                    }
                    done.generation = self.root.generation;
                    return Ok(done);
                }
                if modes.level == Some(statement::Isolation::Serializable) {
                    self.serial_allowed(true)?;
                }
                self.begun = Some((self.root.clone(), false));
                self.levels.begin(modes);
                false
            }
            // Modes named for the transaction or the session; a level changed after the
            // transaction's first read fails the transaction, as PostgreSQL's does.
            Statement::Isolation { modes, at } => {
                let begun = self.begun.is_some();
                let set = (if modes.level == Some(statement::Isolation::Serializable) {
                    self.serial_allowed(at == statement::Isolated::Transaction && begun)
                } else {
                    Ok(())
                })
                .and_then(|_| {
                    self.levels
                        .set(modes, at, begun, !self.savepoints.is_empty())
                });
                if let Err(e) = set {
                    self.aborted = begun;
                    return Err(e);
                }
                false
            }
            Statement::Commit if self.begun.is_none() => {
                done.notices
                    .push("WARNING:  there is no transaction in progress".into());
                false
            }
            Statement::Rollback if self.begun.is_none() => {
                done.notices
                    .push("WARNING:  there is no transaction in progress".into());
                false
            }
            Statement::Commit => {
                // The foreign keys deferred to the commit are checked on the newest root, and a
                // transaction of one snapshot's rows written there; one that fails rolls the
                // transaction back, as PostgreSQL's COMMIT does.
                if let Err(e) = self.commit_checked() {
                    let (began, _) = self.begun.take().expect("a transaction begun");
                    self.root = began;
                    self.ended(false);
                    self.abandon()?;
                    return Err(e);
                }
                let (_, changed) = self.begun.take().expect("a transaction begun");
                self.ended(true);
                changed
            }
            Statement::Rollback => {
                let (root, _) = self
                    .begun
                    .take()
                    .ok_or_else(|| Error::new("no transaction is begun"))?;
                self.root = root;
                self.ended(false);
                self.abandon()?;
                false
            }
            mut st => {
                // A snail marked a replica takes writes from the applier presenting its source
                // alone.
                if !matches!(st, Statement::Constraints { .. }) {
                    if let Err(e) = self.writes_marked(&self.root) {
                        self.failed()?;
                        return Err(e);
                    }
                }
                // A statement that reads and writes takes the transaction's first snapshot; SET
                // CONSTRAINTS takes none.
                if self.begun.is_some() && !matches!(st, Statement::Constraints { .. }) {
                    self.levels.read();
                }
                // Its constants folded before any row is read, as PostgreSQL's planner folds them.
                if matches!(
                    st,
                    Statement::Insert { .. }
                        | Statement::Update { .. }
                        | Statement::Delete { .. }
                        | Statement::Set { .. }
                        | Statement::Returning { .. }
                        | Statement::OnConflict { .. }
                        | Statement::Checked { .. }
                        | Statement::Writes { .. }
                ) {
                    if let Err(e) = super::folded::folded(&self.catalog, text, Some(&*self)) {
                        self.failed()?;
                        return Err(e);
                    }
                }
                self.touched = Touched::default();
                let mark = self.mark();
                // The rows the statement found when it began, which it keeps to if it runs again.
                let mut found = None;
                // A statement that calls the sequence functions runs on a tab, as does an INSERT
                // whose VALUES draws its DEFAULTs inside its question; a run that owes the file is
                // undone, its calls replayed on the file, and the statement runs again over what
                // they came to.
                let runs = match drawn_in_its_question(&st) {
                    true => self.runs().map(Some),
                    false => self.sequence_runs(text),
                };
                let mut runs = match runs {
                    Ok(r) => r,
                    Err(e) => {
                        self.failed()?;
                        return Err(e);
                    }
                };
                // Each run draws from where the statement began.
                let began = self.drawing.stands();
                loop {
                    self.stale = false;
                    self.drawing.put_back(began);
                    let tabbing = runs.as_ref().map(|r| self.tab_for(r));
                    // Its foreign keys are checked as it ends, their actions written first.
                    let written =
                        self.write_rows(st.clone(), &mut done, &mut found)
                            .and_then(|changed| match self.stale {
                                true => Ok(changed),
                                false => self
                                    .exclusions_checked()
                                    .and_then(|fresh| match fresh {
                                        true => self.keys_at_statement_end(),
                                        false => Ok(false),
                                    })
                                    .map(|_| changed),
                            });
                    // A write's RETURNING, read over the rows it changed as it ran last.
                    let returned = match (&written, &st) {
                        (Ok(_), Statement::Returning { write, alias, items }) if !self.stale => {
                            let kept = self.kept.take().unwrap_or_default();
                            Some(self.returned(write, alias, items, kept))
                        }
                        _ => None,
                    };
                    let tab = tabbing.map(super::sequences::Tabbing::ended);
                    if let (Some(r), Some(tab)) = (runs.as_mut(), tab) {
                        if !(self.stale && written.is_ok()) {
                            if tab.owes() {
                                // Run again on the same root, it finds the same rows and makes
                                // them again over what its calls came to.
                                self.back_to(&mark);
                                self.kept = None;
                                found = None;
                                if let Err(e) = self.replay_run(r, &tab) {
                                    self.failed()?;
                                    return Err(e);
                                }
                                continue;
                            }
                            self.took_tab(tab);
                        }
                    }
                    match written {
                        Ok(_) if self.stale => {
                            // A block the statement locked was written since the root it read: it
                            // runs again on the newest root, kept to the rows it found, each in its
                            // newest version and still meeting its conditions, as PostgreSQL checks
                            // a row changed meanwhile again. The blocks it locked stay locked.
                            // A transaction of one snapshot, whose rows are written beside the
                            // newest, comes here only by a key's check it reads no newer than its
                            // block, and is refused.
                            self.back_to(&mark);
                            if self.held {
                                self.failed()?;
                                return Err(super::beside::concurrent_update());
                            }
                            let named = self.root.catalog_named();
                            if self.begun.is_some() {
                                self.renew_in_transaction()?;
                            } else {
                                self.renew()?;
                            }
                            // Where another writer's DDL moved the catalog, it is read again on
                            // the catalog as committed, finding its rows afresh.
                            if self.root.catalog_named() != named {
                                match statement::parse(&self.catalog, text) {
                                    Ok(again) => (st, found) = (again, None),
                                    Err(e) => {
                                        self.failed()?;
                                        return Err(e);
                                    }
                                }
                            }
                        }
                        Ok(changed) => {
                            match returned {
                                Some(Ok(b)) => done.answer = Some(b),
                                Some(Err(e)) => {
                                    self.failed()?;
                                    return Err(e);
                                }
                                None => {}
                            }
                            break changed;
                        }
                        Err(e) => {
                            self.kept = None;
                            self.failed()?;
                            return Err(e);
                        }
                    }
                }
            }
        };
        // What set_config set as the statement ran stands for the session once it has.
        for (name, taken, local) in viewing.changed() {
            self.take_setting(&name, taken, local)?;
        }
        if let Some((_, any)) = self.begun.as_mut() {
            *any |= changed;
        } else if changed || self.serial_records()? {
            // A commit that writes prunes what reads outside a transaction found wasted since.
            if changed {
                match self.prune_unpruned() {
                    Ok(pruned) => done.blocks_pruned = pruned,
                    Err(e) => {
                        self.abandon()?;
                        return Err(e);
                    }
                }
            }
            self.commit()?;
        } else {
            // A statement alone, or a transaction's end, that wrote nothing lets go of what it
            // locked, tested first at SERIALIZABLE.
            let tested = self.serial_tested_alone();
            self.let_go_blocks()?;
            self.serial_end()?;
            tested?;
        }
        done.generation = self.root.generation;
        done.pages_read = self.reader.pages_read() - read0;
        done.pages_written = self.written() - written0;
        done.pages_reused = self.reused - reused0;
        Ok(done)
    }

    /// Begins a transaction that reads one snapshot, as REPEATABLE READ does, on the root `other`
    /// reads: inside a host, a writer's transaction on the generation the host's transaction holds.
    pub fn begin_on(&mut self, other: &Snail) -> Result<Done> {
        let done = self.execute("BEGIN ISOLATION LEVEL REPEATABLE READ")?;
        self.root = other.root.clone();
        self.catalog = other.catalog.clone();
        if let Some((began, _)) = self.begun.as_mut() {
            *began = other.root.clone();
        }
        self.held = true;
        self.levels.read();
        Ok(done)
    }

    /// A question among a writer's statements, answered as `ask_with` answers it; in a
    /// transaction, one that fails fails the transaction.
    fn answered(&mut self, sql: &str, params: &[String]) -> Result<Done> {
        let (read0, written0, reused0) = (self.reader.pages_read(), self.written(), self.reused);
        let question = sql.trim().trim_end_matches(';').trim_end();
        let batch = match self.ask_with(question, None, params) {
            Ok((batch, _)) => batch,
            Err(e) => {
                if self.begun.is_some() && !self.aborted {
                    self.failed()?;
                }
                return Err(e);
            }
        };
        Ok(Done {
            statement: "SELECT",
            rows: batch.num_rows() as u64,
            generation: self.root.generation,
            pages_read: self.reader.pages_read() - read0,
            pages_written: self.written() - written0,
            pages_reused: self.reused - reused0,
            answer: Some(batch),
            ..Default::default()
        })
    }

    /// Marks the snail a replica of `source`, or clears the mark with `None`: a commit of its own,
    /// or the transaction's under way, at its COMMIT. While the mark stands, every writer but the
    /// one presenting itself as `source`'s applier is refused. A snail marked a replica of another
    /// source is refused a mark until it is cleared; marking it as it is marked, or clearing a mark
    /// it does not hold, writes nothing.
    pub fn mark_replica(&mut self, source: Option<&str>) -> Result<Done> {
        if self.writer.is_none() {
            return Err(Error::new(
                "this snail was opened to read; open it to write",
            ));
        }
        if self.aborted {
            return Err(Error::new(ABORTED));
        }
        // Refused in a transaction, it fails the transaction, as a statement refused does.
        let marked = self.marked(source);
        if marked.is_err() && self.begun.is_some() {
            self.failed()?;
        }
        marked
    }

    fn marked(&mut self, source: Option<&str>) -> Result<Done> {
        if source == Some("") {
            return Err(Error::new("a replica's source is named: the name is empty"));
        }
        if self.begun.is_none() && self.rounds > 0 {
            self.renew()?;
        } else if self.begun.is_some() {
            self.renew_in_transaction()?;
        }
        self.rounds += 1;
        let (read0, written0, reused0) = (self.reader.pages_read(), self.written(), self.reused);
        let stands = match &self.marking {
            Some(m) => m.clone(),
            None => self.root.replica.clone(),
        };
        let wanted = source.map(str::to_string);
        if let (Some(stands), Some(new)) = (&stands, &wanted) {
            if stands != new {
                return Err(Error::new(format!(
                    "this snail is marked a replica of '{stands}': clear the mark before marking it \
                     a replica of '{new}'"
                )));
            }
        }
        if stands != wanted {
            self.marking = Some(wanted);
            match self.begun.as_mut() {
                Some((_, any)) => *any = true,
                None => self.commit()?,
            }
        }
        Ok(Done {
            statement: "REPLICA",
            generation: self.root.generation,
            pages_read: self.reader.pages_read() - read0,
            pages_written: self.written() - written0,
            pages_reused: self.reused - reused0,
            ..Default::default()
        })
    }

    /// Refuses a write on a snail marked a replica of a source whose applier this writer does not
    /// present itself as: the mark `root` holds, or the one the statement or the transaction under
    /// way sets or clears.
    pub(super) fn writes_marked(&self, root: &Root) -> Result<()> {
        let mark = match &self.marking {
            Some(m) => m.as_ref(),
            None => root.replica.as_ref(),
        };
        match mark {
            Some(source) if self.applier.as_ref() != Some(source) => Err(Error::new(format!(
                "this snail is marked a replica of '{source}': only the applier presenting \
                 '{source}' writes it, until the mark is cleared"
            ))),
            _ => Ok(()),
        }
    }

    /// The rows an INSERT, UPDATE or DELETE writes, staged. Returns whether it changed any. `found`
    /// is the rows the statement found when it began: given, an INSERT writes them again, and an
    /// UPDATE or a DELETE keeps to the rows it finds now that are those, by the table's key.
    fn write_rows(
        &mut self,
        st: Statement,
        done: &mut Done,
        found: &mut Option<Vec<Row>>,
    ) -> Result<bool> {
        Ok(match st {
            // VALUES rows read through the file's volatile functions alone, each such call's
            // statement seeing the rows written before its own: each row written as it is made.
            Statement::Insert {
                table,
                source: Source::Computed(rows),
            } if found.is_none() && sees_its_write(&self.catalog, &rows) => {
                self.returned_calls.clear();
                let t = self.catalog.tables[table].clone();
                self.computed_typed(&t, &rows)?;
                let one = rows.len() == 1;
                let mut all = Vec::new();
                for row in rows {
                    let made = self.computed(table, vec![row], one)?;
                    let changes: Vec<Change> = made.iter().cloned().map(Change::Insert).collect();
                    self.keep(&changes);
                    let (rewritten, orders) = self.put(table, changes)?;
                    done.rewritten += rewritten;
                    done.orders = done.orders.max(orders);
                    all.extend(made);
                }
                done.rows = all.len() as u64;
                *found = Some(all);
                done.rows > 0
            }
            Statement::Insert { table, source } => {
                let rows = self.proposed(table, source, found)?;
                done.rows = rows.len() as u64;
                let changes: Vec<Change> = rows.into_iter().map(Change::Insert).collect();
                self.keep(&changes);
                (done.rewritten, done.orders) = self.put(table, changes)?;
                done.rows > 0
            }
            Statement::OnConflict {
                insert,
                alias,
                arbiter,
                update,
            } => {
                let Statement::Insert { table, source } = *insert else {
                    unreachable!("ON CONFLICT is an INSERT's")
                };
                let rows = self.proposed(table, source, found)?;
                let changes = self.conflicted(table, &alias, arbiter, update.as_ref(), rows)?;
                done.rows = changes.len() as u64;
                self.keep(&changes);
                (done.rewritten, done.orders) = self.put(table, changes)?;
                done.rows > 0
            }
            Statement::Update { table, find, sets } => {
                let sets = sets
                    .into_iter()
                    .map(|(c, d)| (c, Fill::Literal(d)))
                    .collect();
                self.set(table, find, sets, done, found)?
            }
            Statement::Set { table, find, sets } => self.set(table, find, sets, done, found)?,
            // The write's changes to its own table kept for its RETURNING, afresh each time it runs.
            Statement::Returning { write, .. } => {
                self.kept = Some(Vec::new());
                self.write_rows(*write, done, found)?
            }
            Statement::Writes { writes, question } => self.written_with(writes, &question, done)?,
            // A write through a view: each new row of its table held to the views' CHECK OPTIONs.
            Statement::Checked { write, checks } => {
                let (Statement::Insert { table, .. }
                | Statement::Set { table, .. }
                | Statement::Update { table, .. }
                | Statement::Delete { table, .. }) = write.written()
                else {
                    unreachable!("a write through a view")
                };
                let conditions = checks
                    .iter()
                    .map(|(v, c)| Ok((v.clone(), super::image::parse_expr(c)?)))
                    .collect::<Result<Vec<_>>>()?;
                self.checking = Some((*table, conditions));
                let wrote = self.write_rows(*write, done, found);
                self.checking = None;
                wrote?
            }
            Statement::Delete { table, find } => {
                let (batch, _) = self.answer(&find, None)?;
                let t = &self.catalog.tables[table];
                // A row that USING meets more than once is deleted once.
                let rows = found_rows(t, &stored_of(t, &batch)?)?;
                let once: Vec<Row> = met_once(t, &batch)?
                    .into_iter()
                    .map(|i| rows[i].clone())
                    .collect();
                let kept = self.kept_to(table, &once, found)?;
                let old: Vec<Row> = kept.into_iter().map(|k| once[k].clone()).collect();
                done.rows = old.len() as u64;
                let changes: Vec<Change> = old.into_iter().map(Change::Delete).collect();
                self.keep(&changes);
                (done.rewritten, done.orders) = self.put(table, changes)?;
                done.rows > 0
            }
            Statement::Declare { ddl, .. } => {
                let said = self.declare(&ddl)?;
                done.notices
                    .extend(said.into_iter().map(|n| format!("NOTICE:  {n}")));
                true
            }
            Statement::Constraints { names, deferred } => {
                self.constrained(names, deferred)?;
                false
            }
            Statement::Begin
            | Statement::Commit
            | Statement::Rollback
            | Statement::Setting { .. }
            | Statement::Isolation { .. }
            | Statement::Question { .. }
            | Statement::Session(_) => {
                unreachable!(
                    "a transaction's own statements, a setting and a question write no rows"
                )
            }
        })
    }

    /// An UPDATE: each row found given its columns set, to a value or to its DEFAULT, its stored
    /// generated columns computed again, and its CHECKs checked.
    fn set(
        &mut self,
        table: usize,
        find: String,
        sets: Vec<(usize, Fill)>,
        done: &mut Done,
        found: &mut Option<Vec<Row>>,
    ) -> Result<bool> {
        let t = self.catalog.tables[table].clone();
        for (c, f) in &sets {
            let col = &t.columns[*c];
            let always = col.identity.as_ref().is_some_and(|i| i.always);
            if (col.generated.is_some() || always) && !matches!(f, Fill::Default) {
                return Err(Error::new(format!(
                    "column \"{}\" can only be updated to DEFAULT",
                    col.name
                )));
            }
        }
        // Each expression a column is set to, typed before any row is read, as PostgreSQL types it
        // against the column: read on the finder's relation, by the name it reads the table by.
        let (relation, by) = finder_relation(&find)?;
        // Each written as the pipeline writes a statement's, and typed once, each item in turn,
        // before its type is held to its column's.
        let mut computed: Vec<(usize, sqlparser::ast::Expr)> = Vec::new();
        for (c, f) in &sets {
            if let Fill::Expression(text) = f {
                let e = super::image::parse_expr(text)?;
                computed.push((*c, super::spelled::expr_written(&e)));
            }
        }
        let mut typed = Vec::with_capacity(computed.len());
        if !computed.is_empty() {
            let typing = format!(
                "SELECT {} FROM {relation}",
                computed
                    .iter()
                    .map(|(_, e)| format!("({e})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            let stated = stated_types(&self.catalog, &typing);
            for (k, (c, e)) in computed.iter().enumerate() {
                typed.push(fill::set_typed(&t, e, &by)?);
                let col = &t.columns[*c];
                let from = stated.get(k).copied().flatten();
                if let Some(from) = from.filter(|&from| !assignable(from, col.ty)) {
                    return Err(Error::new(format!(
                        "column \"{}\" is of type {} but expression is of type {}{NOT_ASSIGNED}",
                        col.name,
                        super::typed::type_name(col.ty),
                        super::typed::type_name(from)
                    )));
                }
            }
        }
        let (batch, _) = self.answer(&find, None)?;
        // Under FROM the values set are the answer's columns after the table's, before the row's
        // ctid, each row's paired with it; a row the join meets more than once changed once, by
        // the first values it meets.
        let stored = t.columns.iter().filter(|c| !c.is_virtual()).count();
        let n = batch.num_columns();
        let asked = match n > stored + 1 {
            true => {
                let values = batch.project(&(stored..n - 1).collect::<Vec<_>>())?;
                Some((answer_types(&values)?, rows_of(&values)?))
            }
            false => None,
        };
        let rows = found_rows(&t, &stored_of(&t, &batch)?)?;
        let met = met_once(&t, &batch)?;
        let once: Vec<Row> = met.iter().map(|&i| rows[i].clone()).collect();
        let kept = self.kept_to(table, &once, found)?;
        let old: Vec<(Row, usize)> = kept
            .into_iter()
            .map(|k| (once[k].clone(), met[k]))
            .collect();
        done.rows = old.len() as u64;
        let defaults: Vec<usize> = sets
            .iter()
            .filter(|(_, f)| matches!(f, Fill::Default))
            .map(|(c, _)| *c)
            .collect();
        self.reads_its_writes(&t, old.len(), defaults.iter().map(|&c| (c, old.len())))?;
        let mut drawn = self.draws(&t, defaults.iter().map(|&c| (c, old.len())))?;
        // Values read through the file's volatile functions alone, each such call's statement
        // seeing the rows written before its own: each row written as its values are made.
        let stepwise = computed
            .iter()
            .any(|(_, e)| super::own::reads_relations(&self.catalog, e));
        let mut changes = Vec::with_capacity(old.len());
        for (o, i) in old {
            let mut n = o.clone();
            for (c, f) in &sets {
                n[*c] = match f {
                    Fill::Literal(d) => d.clone(),
                    Fill::Expression(_) => {
                        let at = computed
                            .iter()
                            .position(|(k, _)| k == c)
                            .expect("an expression");
                        match stepwise {
                            true => fill::set_to_reading(
                                &t,
                                &o,
                                *c,
                                &computed[at].1,
                                &by,
                                &typed[at],
                                &*self,
                            )?,
                            false => fill::set_to(&t, &o, *c, &computed[at].1, &by, &typed[at])?,
                        }
                    }
                    Fill::Answer(k) => {
                        let (types, values) = asked.as_ref().expect("values asked");
                        let col = &t.columns[*c];
                        fill::assign(values[i][*k].clone(), types[*k], col.ty, &col.name)?
                    }
                    _ => self.default_of(&t, *c, &mut drawn)?,
                };
            }
            fill::generate(&t, &mut n)?;
            fill::check_domains_reading(&self.catalog.domains, &t, &n, Some(self))?;
            fill::check_reading(&t, &n, Some(self))?;
            fill::without_overlaps(&t, &n)?;
            changes.push(Change::Replace(o, n));
            if stepwise {
                let row = std::mem::take(&mut changes);
                self.keep(&row);
                let (rewritten, orders) = self.put(table, row)?;
                done.rewritten += rewritten;
                done.orders = done.orders.max(orders);
            }
        }
        if !stepwise {
            self.keep(&changes);
            (done.rewritten, done.orders) = self.put(table, changes)?;
        }
        Ok(done.rows > 0)
    }

    /// Keeps a write's changes to its own table where its statement returns rows of them.
    fn keep(&mut self, changes: &[Change]) {
        if let Some(k) = self.kept.as_mut() {
            k.extend_from_slice(changes);
        }
    }

    /// The rows an INSERT proposes: its VALUES or its question's rows, each column filled, kept
    /// where it runs again.
    fn proposed(
        &mut self,
        table: usize,
        source: Source,
        found: &mut Option<Vec<Row>>,
    ) -> Result<Vec<Row>> {
        if found.is_none() {
            self.returned_calls.clear();
        }
        let typed = matches!(source, Source::Query { .. });
        let rows = match (source, found.as_ref()) {
            (_, Some(rows)) => rows.clone(),
            (Source::Values(rows), None) => {
                let given = rows
                    .into_iter()
                    .map(|r| r.into_iter().map(Fill::Literal).collect())
                    .collect();
                self.filled(table, given)?
            }
            (Source::Filled(rows), None) => self.filled(table, rows)?,
            (Source::Computed(rows), None) => {
                let one = rows.len() == 1;
                self.computed(table, rows, one)?
            }
            (Source::Select { question, fill } | Source::Query { question, fill }, None) => {
                let (batch, _) = self.answer(&question, None)?;
                let types = assigned_types(
                    &self.catalog,
                    &question,
                    &batch,
                    table,
                    &answered(&fill),
                    typed,
                )?;
                let calls = calls_made(&batch);
                let given: Vec<(Vec<Fill>, Row)> = rows_of(&batch)?
                    .into_iter()
                    .map(|a| (fill.clone(), a))
                    .collect();
                if calls > 0 {
                    self.returned_calls
                        .extend(given.iter().map(|(_, a)| a[a.len() - calls..].to_vec()));
                }
                self.filled_from(table, given, &types)?
            }
        };
        *found = Some(rows.clone());
        Ok(rows)
    }

    /// An INSERT … ON CONFLICT's changes, as PostgreSQL makes them row by row: a proposed row
    /// looked up on its arbiter among the rows the statement wrote and the rows held; inserted where
    /// none holds its key; where one does, passed over by DO NOTHING, or set by DO UPDATE over the
    /// row held and EXCLUDED, the proposed one, where its WHERE holds. A row the statement wrote
    /// met again by DO UPDATE is refused.
    fn conflicted(
        &mut self,
        table: usize,
        alias: &str,
        arbiter: Option<usize>,
        update: Option<&(Vec<(usize, Fill)>, Option<String>)>,
        rows: Vec<Row>,
    ) -> Result<Vec<Change>> {
        let t = self.catalog.tables[table].clone();
        let orders: Vec<usize> = match arbiter {
            Some(o) => vec![o],
            None => (0..t.orders.len())
                .filter(|&o| t.orders[o].unique && t.orders[o].predicate.is_none())
                .collect(),
        };
        // A key of an order, none where it holds a NULL its order keeps distinct.
        let key = |r: &Row, o: &Order| -> Option<Vec<Datum>> {
            let k: Vec<Datum> = o.levels.iter().map(|l| r[l.column()].clone()).collect();
            (!o.nulls_distinct || !k.iter().any(|d| matches!(d, Datum::Null))).then_some(k)
        };
        let mut sets: Vec<(usize, Fill, Option<sqlparser::ast::Expr>)> = Vec::new();
        let mut selection = None;
        if let Some((given, w)) = update {
            for (c, f) in given {
                let col = &t.columns[*c];
                let always = col.identity.as_ref().is_some_and(|i| i.always);
                if col.generated.is_some() || always {
                    return Err(Error::new(format!(
                        "column \"{}\" can only be updated to DEFAULT",
                        col.name
                    )));
                }
                let e = match f {
                    Fill::Expression(text) => Some(super::image::parse_expr(text)?),
                    Fill::Literal(_) => None,
                    _ => return Err(Error::new("DEFAULT in ON CONFLICT DO UPDATE is not written")),
                };
                sets.push((*c, f.clone(), e));
            }
            selection = w.as_deref().map(super::image::parse_expr).transpose()?;
        }
        let mut changes = Vec::new();
        let mut written: Vec<Row> = Vec::new();
        let mut replaced: Vec<Row> = Vec::new();
        for r in rows {
            let mut hit: Option<Option<Row>> = None;
            for &oi in &orders {
                let o = &t.orders[oi];
                let Some(k) = key(&r, o) else {
                    continue;
                };
                if written.iter().any(|w| key(w, o).as_ref() == Some(&k)) {
                    hit = Some(None);
                    break;
                }
                let columns: Vec<usize> = o.levels.iter().map(|l| l.column()).collect();
                let held = self.rows_where(table, &columns, &k, false)?;
                if let Some(h) = held.into_iter().find(|h| !replaced.contains(h)) {
                    hit = Some(Some(h));
                    break;
                }
            }
            match (hit, update) {
                (None, _) => {
                    written.push(r.clone());
                    changes.push(Change::Insert(r));
                }
                (Some(_), None) => {}
                (Some(None), Some(_)) => {
                    return Err(Error::new(
                        "ON CONFLICT DO UPDATE command cannot affect row a second time",
                    ))
                }
                (Some(Some(h)), Some(_)) => {
                    if let Some(w) = &selection {
                        if !matches!(
                            fill::on_conflict(&t, &h, &r, w, alias)?,
                            super::Value::Bool(true)
                        ) {
                            continue;
                        }
                    }
                    let mut n = h.clone();
                    for (c, f, e) in &sets {
                        n[*c] = match (f, e) {
                            (Fill::Literal(d), _) => d.clone(),
                            (_, Some(e)) => fill::on_conflict_set(&t, &h, &r, *c, e, alias)?,
                            _ => unreachable!("a value or an expression"),
                        };
                    }
                    fill::generate(&t, &mut n)?;
                    fill::check_domains_reading(&self.catalog.domains, &t, &n, Some(self))?;
                    fill::check_reading(&t, &n, Some(self))?;
                    fill::without_overlaps(&t, &n)?;
                    replaced.push(h.clone());
                    written.push(n.clone());
                    changes.push(Change::Replace(h, n));
                }
            }
        }
        Ok(changes)
    }

    /// Each row a write lands in a table with no column of a B-tree numbered as it arrives: the
    /// table's last number's plus one, in the statement's order. A row an UPDATE writes keeps the
    /// number it found.
    fn arrived(&mut self, table: usize, changes: &mut [Change]) -> Result<()> {
        let t = &self.catalog.tables[table];
        let Some(a) = t.arrival() else {
            return Ok(());
        };
        if !changes
            .iter()
            .any(|c| matches!(c, Change::Insert(r) if r[a] == Datum::Null))
        {
            return Ok(());
        }
        let last = format!(
            "SELECT max(\"{}\") FROM {}",
            crate::catalog::ARRIVAL,
            t.name
        );
        let (b, _) = self.answer(&last, None)?;
        let mut next = match rows_of(&b)?.first().map(|r| &r[0]) {
            Some(Datum::Int(n)) => *n,
            _ => 0,
        };
        for c in changes {
            if let Change::Insert(r) = c {
                if r[a] == Datum::Null {
                    next += 1;
                    r[a] = Datum::Int(next);
                }
            }
        }
        Ok(())
    }

    /// A write's changes applied, or held unapplied for a write inside WITH until every write of
    /// its statement has found its rows.
    fn put(&mut self, table: usize, mut changes: Vec<Change>) -> Result<(u64, u64)> {
        self.arrived(table, &mut changes)?;
        // Each new row written through a view meets the conditions of its CHECK OPTIONs.
        if let Some((through, conditions)) = &self.checking {
            if *through == table {
                let t = &self.catalog.tables[table];
                for c in &changes {
                    if let Change::Insert(n) | Change::Replace(_, n) = c {
                        for (view, condition) in conditions {
                            fill::check_option(t, n, view, condition)?;
                        }
                    }
                }
            }
        }
        match self.dry.as_mut() {
            Some(held) => {
                held.push((table, changes));
                Ok((0, 0))
            }
            None => self.apply(table, changes),
        }
    }

    /// A question whose WITH holds writes: each write finds its rows and makes its changes on the
    /// statement's one snapshot, held unapplied, its RETURNING read over them; the question is
    /// asked on that snapshot, each write's query read as the rows it returns; then every write's
    /// changes apply, in the WITH's order.
    fn written_with(
        &mut self,
        writes: Vec<(String, Statement)>,
        question: &str,
        done: &mut Done,
    ) -> Result<bool> {
        let mut held = Vec::new();
        let mut returned: Vec<(String, Option<RecordBatch>)> = Vec::new();
        for (name, st) in writes {
            let mut own = Done::default();
            self.dry = Some(Vec::new());
            let ran = match &st {
                Statement::Returning { write, alias, items } => {
                    self.kept = Some(Vec::new());
                    let ran = self.write_rows((**write).clone(), &mut own, &mut None);
                    let kept = self.kept.take().unwrap_or_default();
                    ran.and_then(|_| self.returned(write, alias, items, kept).map(Some))
                }
                st => self.write_rows(st.clone(), &mut own, &mut None).map(|_| None),
            };
            held.extend(self.dry.take().unwrap_or_default());
            returned.push((name, ran?));
        }
        // Each write's query read as a function handed for the question alone, of the rows it
        // returns; one returning none, refused where the question reads it, as PostgreSQL's
        // parser refuses it.
        let mut q = match super::image::parse_sql(question)?.pop() {
            Some(sqlparser::ast::Statement::Query(q)) => q,
            _ => unreachable!("a question"),
        };
        let mut handed = self.functions.clone();
        let mut read: Vec<String> = Vec::new();
        let _ = sqlparser::ast::visit_relations(&q, |n| {
            read.push(crate::catalog::object_name(n));
            std::ops::ControlFlow::<()>::Continue(())
        });
        let with = q.with.as_mut().expect("a WITH");
        let mut kept_ctes = Vec::new();
        for (k, mut cte) in std::mem::take(&mut with.cte_tables).into_iter().enumerate() {
            let Some((name, batch)) = returned
                .iter()
                .find(|(n, _)| *n == crate::catalog::ident(&cte.alias.name))
                .filter(|_| statement::writes_query(&cte.query))
            else {
                kept_ctes.push(cte);
                continue;
            };
            let Some(batch) = batch else {
                if read.iter().any(|r| r == name) {
                    return Err(Error::new(format!(
                        "WITH query \"{name}\" does not have a RETURNING clause"
                    )));
                }
                continue;
            };
            let types = answer_types(batch)?;
            let schema = batch.schema();
            let named: Vec<(&str, ColumnType)> = schema
                .fields()
                .iter()
                .zip(&types)
                .map(|(f, ty)| (f.name().as_str(), *ty))
                .collect();
            let row = super::handed::row_type(&format!("{KEPT}with{k}"), &named);
            let rows = rows_of(batch)?
                .iter()
                .map(|r| {
                    super::Value::row(
                        row,
                        r.iter().zip(&types).map(|(d, ty)| fill::value_of(d, *ty)).collect(),
                    )
                })
                .collect::<Result<Vec<_>>>()?;
            let f = format!("{KEPT}with{k}");
            super::handed::hand(
                &mut handed,
                &self.catalog,
                super::handed::Function::set_returning(&f, &[], row, move |_| Ok(rows.clone())),
            )?;
            let body = format!("SELECT * FROM {}()", quoted(&f));
            match super::image::parse_sql(&body)?.pop() {
                Some(sqlparser::ast::Statement::Query(b)) => cte.query = b,
                _ => unreachable!("a question"),
            }
            kept_ctes.push(cte);
        }
        with.cte_tables = kept_ctes;
        if with.cte_tables.is_empty() {
            q.with = None;
        }
        {
            let _held = super::handed::holding(&handed);
            let (batch, _) = self.answer(&q.to_string(), None)?;
            done.rows = batch.num_rows() as u64;
            done.answer = Some(batch);
        }
        let mut changed = false;
        for (table, changes) in held {
            changed |= !changes.is_empty();
            let (rewritten, orders) = self.apply(table, changes)?;
            done.rewritten += rewritten;
            done.orders += orders;
        }
        Ok(changed)
    }

    /// A write's RETURNING: its list read as a question over the rows it changed, by the name
    /// `alias` reads its table, each row its table's columns as the write left them (as it found
    /// them, for a DELETE) and, under `old` and `new`, as it found them and as it left them, NULL
    /// where it found none or left none, as PostgreSQL 18 reads them.
    pub(super) fn returned(
        &self,
        write: &Statement,
        alias: &str,
        items: &[sqlparser::ast::SelectItem],
        kept: Vec<Change>,
    ) -> Result<RecordBatch> {
        let (Statement::Insert { table, .. }
        | Statement::Set { table, .. }
        | Statement::Update { table, .. }
        | Statement::Delete { table, .. }) = write.written()
        else {
            unreachable!("a write returns rows")
        };
        let t = &self.catalog.tables[*table];
        let deleting = matches!(write.written(), Statement::Delete { .. });
        // The sequence calls each row's question made for the list, read as the row's.
        let calls = super::statement::returned_calls(items);
        let made = (!calls.is_empty()
            && self.returned_calls.len() == kept.len()
            && self.returned_calls.iter().all(|v| v.len() == calls.len()))
        .then_some(&self.returned_calls);
        let mut fields: Vec<(String, ColumnType)> = Vec::new();
        for side in [None, Some("old"), Some("new")] {
            for c in &t.columns {
                fields.push((kept_name(side, &c.name), c.ty));
            }
        }
        if made.is_some() {
            fields.extend(
                (0..calls.len()).map(|j| (format!("{RETURNED_CALL}{j}"), ColumnType::Int8)),
            );
        }
        let named: Vec<(&str, ColumnType)> = fields.iter().map(|(n, ty)| (n.as_str(), *ty)).collect();
        let row = super::handed::row_type(&format!("{KEPT}returned"), &named);
        let values = |r: Option<&Row>| -> Result<Vec<super::Value>> {
            Ok(match r {
                None => vec![super::Value::Null; t.columns.len()],
                Some(r) => fill::with_virtual(t, r)?
                    .iter()
                    .zip(&t.columns)
                    .map(|(d, c)| fill::value_of(d, c.ty))
                    .collect(),
            })
        };
        let mut rows = Vec::with_capacity(kept.len());
        for (i, c) in kept.iter().enumerate() {
            let (old, new) = match c {
                Change::Insert(n) => (None, Some(n)),
                Change::Delete(o) => (Some(o), None),
                Change::Replace(o, n) => (Some(o), Some(n)),
            };
            let mut v = values(if deleting { old } else { new })?;
            v.extend(values(old)?);
            v.extend(values(new)?);
            if let Some(made) = made {
                v.extend(made[i].iter().map(|d| fill::value_of(d, ColumnType::Int8)));
            }
            rows.push(super::Value::row(row, v)?);
        }
        let items = match made {
            Some(_) => called_as_made(items),
            None => items.to_vec(),
        };
        let list = returning_list(t, alias, &items)?;
        let name = format!("{KEPT}returned");
        let f = super::handed::Function::set_returning(&name, &[], row, move |_| Ok(rows.clone()));
        let mut handed = self.functions.clone();
        super::handed::hand(&mut handed, &self.catalog, f)?;
        let _held = super::handed::holding(&handed);
        let (batch, _) = self.answer(
            &format!("SELECT {list} FROM {}() AS {}", quoted(&name), quoted(alias)),
            None,
        )?;
        Ok(batch)
    }

    /// The rows an INSERT gives, each cell a value or its column's DEFAULT: the DEFAULTs filled,
    /// a sequence's values drawn once for the statement, the stored generated columns computed and
    /// the CHECKs checked. A value given to a generated column, or to an identity GENERATED ALWAYS,
    /// is refused, as PostgreSQL refuses it.
    fn filled(&mut self, table: usize, rows: Vec<Vec<Fill>>) -> Result<Vec<Row>> {
        let given = rows.into_iter().map(|r| (r, Row::new())).collect();
        self.filled_from(table, given, &[])
    }

    /// The rows an INSERT gives, as `filled` fills them, each beside the row its question
    /// answered, of the answer's `types`: a cell the question answers, or a DEFAULT it drew, read
    /// from it.
    fn filled_from(
        &mut self,
        table: usize,
        rows: Vec<(Vec<Fill>, Row)>,
        types: &[ColumnType],
    ) -> Result<Vec<Row>> {
        let t = self.catalog.tables[table].clone();
        given_refused(&t, rows.iter().map(|(r, _)| r))?;
        self.reads_its_writes(&t, rows.len(), defaults_taken(&t, rows.iter().map(|(r, _)| r)))?;
        let mut drawn = self.draws(&t, defaults_taken(&t, rows.iter().map(|(r, _)| r)))?;
        let mut out = Vec::with_capacity(rows.len());
        for (r, a) in rows {
            out.push(self.row_filled(&t, r, &a, types, &mut drawn)?);
        }
        Ok(out)
    }

    /// The rows a VALUES list gives where a cell is an expression, as 18.6 makes them row by row:
    /// every row's expressions typed against their columns first; then each row's question answered
    /// as the reader answers one, its expressions computed and its DEFAULTs drawn in their order
    /// among its calls, and the row filled and checked before the next is computed. One row is a
    /// select list (`one`): each row its question answers is a row given, none where it answers none.
    fn computed(
        &mut self,
        table: usize,
        rows: Vec<(Vec<Fill>, Option<String>)>,
        one: bool,
    ) -> Result<Vec<Row>> {
        let t = self.catalog.tables[table].clone();
        self.computed_typed(&t, &rows)?;
        self.reads_its_writes(&t, rows.len(), defaults_taken(&t, rows.iter().map(|(r, _)| r)))?;
        let mut drawn = self.draws(&t, defaults_taken(&t, rows.iter().map(|(r, _)| r)))?;
        let mut out = Vec::with_capacity(rows.len());
        for (r, q) in rows {
            let (answers, types, calls) = match q {
                None => (vec![Row::new()], Vec::new(), 0),
                Some(q) => {
                    let (batch, _) = self.answer(&q, None)?;
                    let types =
                        assigned_types(&self.catalog, &q, &batch, table, &answered(&r), false)?;
                    let mut answers = rows_of(&batch)?;
                    if !one {
                        answers.truncate(1);
                        if answers.is_empty() {
                            return Err(Error::new("a VALUES row's question answers its row"));
                        }
                    }
                    (answers, types, calls_made(&batch))
                }
            };
            for a in answers {
                if calls > 0 {
                    self.returned_calls.push(a[a.len() - calls..].to_vec());
                }
                out.push(self.row_filled(&t, r.clone(), &a, &types, &mut drawn)?);
            }
        }
        Ok(out)
    }

    /// Refuses a write of several rows into `t` whose CHECK, domain, or DEFAULT of a column in
    /// `defaults` (each column and the rows taking it) calls a function whose statement reads `t`
    /// from a VOLATILE function: 18.6's sees the rows the write made before each, which the snail
    /// writes once every row is made.
    fn reads_its_writes(
        &self,
        t: &Table,
        rows: usize,
        defaults: impl Iterator<Item = (usize, usize)>,
    ) -> Result<()> {
        if rows < 2 || self.catalog.functions.is_empty() {
            return Ok(());
        }
        let taking: Vec<usize> = defaults.filter(|&(_, n)| n > 0).map(|(c, _)| c).collect();
        let mut exprs: Vec<&sqlparser::ast::Expr> =
            t.checks.iter().filter(|k| k.enforced).map(|k| &k.expr).collect();
        for (c, col) in t.columns.iter().enumerate() {
            let takes = taking.contains(&c);
            if takes {
                exprs.extend(col.default.as_ref());
            }
            let mut named = col.domain.as_deref();
            while let Some(d) = named.and_then(|n| self.catalog.domains.iter().find(|d| d.name == n)) {
                exprs.extend(d.checks.iter().map(|k| &k.check));
                exprs.extend(d.default.as_ref().filter(|_| takes && col.default.is_none()));
                named = Some(&d.base);
            }
        }
        match exprs
            .into_iter()
            .find_map(|e| super::own::reads_written(&self.catalog, e, &t.name))
        {
            Some(g) => Err(Error::new(format!(
                "{g} reads {}, which the statement writes: a VOLATILE function's statement in a \
                 CHECK, a domain or a DEFAULT sees the rows written before its own, and is called \
                 only where the statement writes one row",
                t.name
            ))),
            None => Ok(()),
        }
    }

    /// A VALUES list's rows given and typed against their columns, as `computed` reads them before
    /// any row is computed.
    fn computed_typed(&self, t: &Table, rows: &[(Vec<Fill>, Option<String>)]) -> Result<()> {
        given_refused(t, rows.iter().map(|(r, _)| r))?;
        for (r, q) in rows {
            let Some(q) = q else {
                continue;
            };
            let stated = stated_types(&self.catalog, q);
            for (c, k) in answered(r) {
                let col = &t.columns[c];
                if let Some(from) = stated
                    .get(k)
                    .copied()
                    .flatten()
                    .filter(|&from| !assignable(from, col.ty))
                {
                    return Err(not_assigned(col, from));
                }
            }
        }
        Ok(())
    }

    /// One row an INSERT gives, each cell filled: a value; the column of `answer`, of `types`, its
    /// question gave it, or the DEFAULT the question drew; else its DEFAULT, with the values drawn
    /// for the statement (`drawn`). Its stored generated columns are computed, and its domains,
    /// CHECKs and temporal keys checked.
    fn row_filled(
        &self,
        t: &Table,
        r: Vec<Fill>,
        answer: &[Datum],
        types: &[ColumnType],
        drawn: &mut std::collections::HashMap<String, std::collections::VecDeque<i64>>,
    ) -> Result<Row> {
        let mut row = Vec::with_capacity(r.len());
        for (c, f) in r.into_iter().enumerate() {
            let col = &t.columns[c];
            row.push(match f {
                Fill::Literal(d) => d,
                Fill::Answer(k) | Fill::Drawn(k) => {
                    fill::assign(answer[k].clone(), types[k], col.ty, &col.name)?
                }
                _ => self.default_of(t, c, drawn)?,
            });
        }
        fill::generate(t, &mut row)?;
        fill::check_domains_reading(&self.catalog.domains, t, &row, Some(self))?;
        fill::check_reading(t, &row, Some(self))?;
        fill::without_overlaps(t, &row)?;
        Ok(row)
    }

    /// The values each column's sequence hands out for the statement, drawn at once: `counts` the
    /// rows that take each column's DEFAULT.
    pub(super) fn draws(
        &mut self,
        t: &Table,
        counts: impl Iterator<Item = (usize, usize)>,
    ) -> Result<std::collections::HashMap<String, std::collections::VecDeque<i64>>> {
        let mut need: Vec<(String, usize)> = Vec::new();
        for (c, n) in counts {
            if n == 0 {
                continue;
            }
            if let Some(s) = sequence_of(&self.catalog, &t.columns[c]) {
                match need.iter_mut().find(|(x, _)| *x == s) {
                    Some(e) => e.1 += n,
                    None => need.push((s, n)),
                }
            }
        }
        let mut out = std::collections::HashMap::new();
        for (s, n) in need {
            // A statement that calls the sequence functions draws on its tab, in its calls' order.
            let values = match super::sequences::defaults(&s, n) {
                Some(drawn) => drawn?,
                None => self.draw(&s, n)?,
            };
            out.insert(s, values.into());
        }
        Ok(out)
    }

    /// A column's DEFAULT for one row: its identity's or its nextval's next value drawn, the clock,
    /// a literal, or an expression computed; NULL where it has none.
    pub(super) fn default_of(
        &self,
        t: &Table,
        c: usize,
        drawn: &mut std::collections::HashMap<String, std::collections::VecDeque<i64>>,
    ) -> Result<Datum> {
        let col = &t.columns[c];
        if col.generated.is_some() {
            return Ok(Datum::Null);
        }
        if let Some(s) = sequence_of(&self.catalog, col) {
            let v = drawn
                .get_mut(&s)
                .and_then(|q| q.pop_front())
                .expect("a value drawn for each DEFAULT");
            return fill::assign(Datum::Int(v), ColumnType::Int8, col.ty, &col.name);
        }
        // A column of a domain with no DEFAULT of its own takes the domain's.
        let domain_default = col
            .domain
            .as_ref()
            .and_then(|d| fill::domain_default(&self.catalog.domains, d));
        let Some(e) = col.default.as_ref().or(domain_default) else {
            return Ok(Datum::Null);
        };
        if let Some(name) = clock_of(e) {
            let at = match name.as_str() {
                "statement_timestamp" => self.clock.1,
                "clock_timestamp" => wall_micros(),
                _ => self.clock.0,
            };
            return fill::assign(Datum::Int(at), ColumnType::Timestamptz, col.ty, &col.name);
        }
        if let Some(v) = uuid_of(e) {
            return fill::assign(Datum::Wide(v?), ColumnType::Uuid, col.ty, &col.name);
        }
        if statement::is_literal(e) || crate::composed::constructs(col.ty, e) {
            return statement::datum(e, t, c);
        }
        // A call of the file's function whose body reads relations asks the snail written.
        fill::assign_value(
            super::pipeline::eval_on_reading(
                e,
                &[],
                &[],
                t.release,
                &super::pipeline::Casings::under(t),
                Some(self),
            )?,
            col.ty,
            &col.name,
        )
    }

    /// The values a sequence hands this writer: those its cache holds first, then a run of CACHE
    /// values at a time drawn at the root's turn, never given back.
    fn draw(&mut self, name: &str, n: usize) -> Result<Vec<i64>> {
        match self.draw_upto(name, n)? {
            (values, None) => Ok(values),
            (_, Some(refused)) => Err(Error::new(refused)),
        }
    }

    /// `n` values drawn as `draw` draws them; where the sequence runs out first, those it handed
    /// before its end, with the words the next was refused in. Every value handed is used up, and
    /// the sequence stands at its end, as PostgreSQL's statement leaves it.
    fn draw_upto(&mut self, name: &str, n: usize) -> Result<(Vec<i64>, Option<String>)> {
        if self
            .declared
            .as_ref()
            .is_some_and(|d| d.resets.iter().any(|r| r == name))
        {
            return Err(Error::new(format!(
                "{name} is declared in this transaction, and hands out values once it is committed"
            )));
        }
        let s = self
            .catalog
            .sequence(name)
            .cloned()
            .ok_or_else(|| Error::new(format!("relation \"{name}\" does not exist")))?;
        let held = self.handed.entry(name.to_string()).or_default();
        let mut out: Vec<i64> = Vec::with_capacity(n);
        while out.len() < n {
            match held.pop_front() {
                Some(v) => out.push(v),
                None => break,
            }
        }
        let mut refused = None;
        if out.len() < n {
            let missing = n - out.len();
            match self.advance(&s, missing) {
                Ok((fetched, ended)) => {
                    let used = missing.min(fetched.len());
                    out.extend_from_slice(&fetched[..used]);
                    self.handed
                        .entry(name.to_string())
                        .or_default()
                        .extend(fetched[used..].iter().copied());
                    refused = ended;
                }
                Err(e) if e.0.starts_with("nextval: reached ") => refused = Some(e.0),
                Err(e) => {
                    // The values held stay held where the file could not be written.
                    let held = self.handed.entry(name.to_string()).or_default();
                    for v in out.into_iter().rev() {
                        held.push_front(v);
                    }
                    return Err(e);
                }
            }
        }
        if let Some(&v) = out.last() {
            let oid = super::sequences::sequence_oid(&self.catalog, name).unwrap_or(0);
            self.drawn.drew(name, oid, v);
        }
        Ok((out, refused))
    }

    /// setval at the root's turn, a commit of its own never rolled back: the sequence stands on
    /// `v`, handed out where `called`, and the values it handed this writer are thrown away.
    fn set_sequence(&mut self, name: &str, v: i64, called: bool) -> Result<()> {
        if self
            .declared
            .as_ref()
            .is_some_and(|d| d.resets.iter().any(|r| r == name))
        {
            return Err(Error::new(format!(
                "{name} is declared in this transaction, and is set once it is committed"
            )));
        }
        self.at_sequences_turn(|states| {
            states.retain(|x| x.0 != name);
            states.push((name.to_string(), v, called));
            Ok(())
        })?;
        self.handed.remove(name);
        Ok(())
    }

    /// The calls a statement's run made that only the file could answer, made on the file in their
    /// order, each draw from what the session holds and then at the root's turn, each set committed:
    /// what each came to, up to and with the first refused or the first that came to other than the
    /// run was answered, after which the statement's course may be another. A draw's values past
    /// that one stay the session's, as its CACHE's do. `stands` keeps where each stands after.
    fn replayed(
        &mut self,
        fresh: &[super::sequences::Fresh],
        stands: &mut std::collections::HashMap<String, (i64, bool)>,
    ) -> Result<Vec<(super::sequences::Op, super::sequences::Outcome)>> {
        use super::sequences::Op;
        let mut out = Vec::with_capacity(fresh.len());
        let mut i = 0;
        while i < fresh.len() {
            let f = &fresh[i];
            match &f.op {
                Op::Draw(s) => {
                    // Draws of one sequence in a row, drawn at once.
                    let n = fresh[i..].iter().take_while(|g| g.op == f.op).count();
                    let (values, refused) = self.drawn_on_file(s, n)?;
                    let parted = values
                        .iter()
                        .zip(&fresh[i..])
                        .position(|(v, g)| g.outcome != Ok(*v));
                    let used = parted.map_or(values.len(), |k| k + 1);
                    out.extend(values[..used].iter().map(|&v| (f.op.clone(), Ok(v))));
                    let held = self.handed.entry(s.clone()).or_default();
                    for &v in values[used..].iter().rev() {
                        held.push_front(v);
                    }
                    // The file stands on the last value it handed the session.
                    if let Some(&v) = held.back().or(values.last()) {
                        stands.insert(s.clone(), (v, true));
                    }
                    if parted.is_some() {
                        return Ok(out);
                    }
                    if let Some(e) = refused {
                        out.push((f.op.clone(), Err(e)));
                        return Ok(out);
                    }
                    i += n;
                    continue;
                }
                Op::Set(s, v, called) => {
                    self.set_sequence(s, *v, *called)?;
                    stands.insert(s.clone(), (*v, *called));
                    out.push((f.op.clone(), Ok(*v)));
                }
            }
            i += 1;
        }
        Ok(out)
    }

    /// `n` values of a sequence drawn for a replay, at once; where it runs out first, those drawn
    /// before its end stand, with the words it was refused in.
    fn drawn_on_file(&mut self, s: &str, n: usize) -> Result<(Vec<i64>, Option<String>)> {
        self.draw_upto(s, n)
    }

    /// The runs of a statement whose text names the sequence functions, itself or through a
    /// function of the file it calls; none for any other.
    pub(super) fn sequence_runs(&self, sql: &str) -> Result<Option<Runs>> {
        if !super::sequences::names(sql)
            && !super::own::names_through(&self.catalog, sql, super::sequences::names)
        {
            return Ok(None);
        }
        self.runs().map(Some)
    }

    /// A statement's runs on a tab, from what the session holds.
    fn runs(&self) -> Result<Runs> {
        Ok(Runs {
            start: self.drawn.clone(),
            held: self.handed.clone(),
            stands: self.sequences_standing()?,
            script: Vec::new(),
            replays: 0,
        })
    }

    /// The tab a run of the statement is lent: what the session holds, and what the replays so
    /// far came to.
    pub(super) fn tab_for(&self, r: &Runs) -> super::sequences::Tabbing {
        let read_only = self.writer.is_none() || self.levels.read_only(self.begun.is_some());
        super::sequences::tabbing(super::sequences::Tab::lent(super::sequences::Lent {
            drawn: r.start.clone(),
            held: &self.handed,
            held_at_start: &r.held,
            stands: &r.stands,
            catalog: &self.catalog,
            read_only,
            script: r.script.clone(),
        }))
    }

    /// A run's calls that only the file can answer, replayed on it: the next run answers them from
    /// what they came to.
    pub(super) fn replay_run(&mut self, r: &mut Runs, tab: &super::sequences::Tab) -> Result<()> {
        r.replays += 1;
        if r.replays > REPLAYS {
            return Err(Error::new(format!(
                "the sequences this statement draws from moved under it on each of {REPLAYS} \
                 runs; run it again"
            )));
        }
        let replayed = self.replayed(tab.fresh(), &mut r.stands)?;
        r.script.truncate(tab.matched());
        r.script.extend(replayed);
        Ok(())
    }

    /// Where each sequence stands as pg_sequence_last_value reads it: on the newest root, as
    /// PostgreSQL reads a sequence's own row whatever the transaction's snapshot.
    pub(super) fn sequences_newest(
        &self,
    ) -> Result<std::collections::HashMap<String, (i64, bool)>> {
        let newest = self.reader.peek_root()?;
        Ok(self
            .reader
            .sequence_states(&newest)?
            .into_iter()
            .map(|(n, v, c)| (n, (v, c)))
            .collect())
    }

    /// What the run that stands left the session: its `currval`s, the sequence it drew from last,
    /// and the values it drew of those the session held.
    pub(super) fn took_tab(&mut self, tab: super::sequences::Tab) {
        self.drawn = tab.drawn().clone();
        self.handed = tab.held();
    }

    /// Where each sequence stands on the root this statement reads, those no draw has moved
    /// standing where the DDL leaves them.
    pub(super) fn sequences_standing(
        &self,
    ) -> Result<std::collections::HashMap<String, (i64, bool)>> {
        Ok(self
            .reader
            .sequence_states(&self.root)?
            .into_iter()
            .map(|(n, v, c)| (n, (v, c)))
            .collect())
    }

    /// Advances a sequence at the root's turn, a commit of its own, as PostgreSQL's nextval is never
    /// rolled back: a run at a time, as nextval fetches one (`sequences::run_from`), until the runs
    /// hold `need` values; the sequence stands on the last value fetched. Where it runs out first,
    /// the runs before its end, with the words the next was refused in; refused where it runs out
    /// before any.
    fn advance(
        &mut self,
        s: &crate::catalog::Sequence,
        need: usize,
    ) -> Result<(Vec<i64>, Option<String>)> {
        self.at_sequences_turn(|states| {
            let (mut last, mut called) = states
                .iter()
                .find(|x| x.0 == s.name)
                .map(|x| (x.1, x.2))
                .unwrap_or((s.last_value, s.is_called));
            let mut out = Vec::with_capacity(need);
            let mut ended = None;
            while out.len() < need {
                match super::sequences::next(s, (last, called)) {
                    Ok(first) => {
                        let run = super::sequences::run_from(s, first);
                        last = *run.last().expect("a run holds the value it begins with");
                        called = true;
                        out.extend(run);
                    }
                    Err(e) if out.is_empty() => return Err(Error::new(e)),
                    Err(e) => {
                        ended = Some(e);
                        break;
                    }
                }
            }
            states.retain(|x| x.0 != s.name);
            states.push((s.name.clone(), last, true));
            Ok((out, ended))
        })
    }

    /// Moves where the sequences stand at the root's turn, a commit of its own: `f` is given their
    /// states on the newest root to move.
    fn at_sequences_turn<T>(
        &mut self,
        f: impl FnOnce(&mut Vec<crate::file::SequenceState>) -> Result<T>,
    ) -> Result<T> {
        let mut w = self
            .writer
            .take()
            .ok_or_else(|| Error::new("this snail was opened to read; open it to write"))?;
        let turn = lock::take_root_turn(w.file());
        let moved = turn.and_then(|_| {
            let root = self.reader.peek_root()?;
            let mut states = self.reader.sequence_states(&root)?;
            let out = f(&mut states)?;
            self.advancing = Some(states);
            let rebased = std::mem::take(&mut self.rebased);
            let declared = self.declared.take();
            let committed = self.commit_with(&mut w);
            self.rebased = rebased;
            self.declared = declared;
            self.advancing = None;
            committed.map(|_| out)
        });
        let given = lock::give_root_turn(w.file());
        self.writer = Some(w);
        let out = moved?;
        given?;
        Ok(out)
    }

    /// Takes `n` OIDs from the counter every writer of the file shares, at the root's turn, a commit
    /// of its own, as PostgreSQL's are never given back: from the newest root's counter, or where it
    /// names none from where its catalog's text goes on, `on` where that is the catalog `from` the
    /// statement was declared on. Returns the first.
    fn at_oids_turn(&mut self, n: u32, from: (Extent, u64), on: u32) -> Result<u32> {
        let mut w = self
            .writer
            .take()
            .ok_or_else(|| Error::new("this snail was opened to read; open it to write"))?;
        let turn = lock::take_root_turn(w.file());
        let taken = turn.and_then(|_| {
            let root = self.reader.peek_root()?;
            let first = match root.next_oid {
                0 if root.catalog_named() == from => on,
                0 => Snail::catalog(&self.reader, &root)?.oids.next(),
                next => next,
            };
            self.taking = Some(first + n);
            let rebased = std::mem::take(&mut self.rebased);
            let declared = self.declared.take();
            let committed = self.commit_with(&mut w);
            self.rebased = rebased;
            self.declared = declared;
            self.taking = None;
            committed.map(|_| first)
        });
        let given = lock::give_root_turn(w.file());
        self.writer = Some(w);
        let first = taken?;
        given?;
        Ok(first)
    }

    /// The rows an UPDATE or a DELETE finds, by their places in `now`: the first time, every one,
    /// kept as `found`; run again, only those whose key, on the table's first unique order of plain
    /// NOT NULL columns or else every column, is a key it found the first time, and of the rows
    /// sharing a key no more than it found then: a row alike written since is not one it found.
    /// With no unique key, a row found that an update replaced since is followed to a row standing
    /// in its place, one for each replaced.
    fn kept_to(&self, ti: usize, now: &[Row], found: &mut Option<Vec<Row>>) -> Result<Vec<usize>> {
        let Some(first) = found.as_ref() else {
            *found = Some(now.to_vec());
            return Ok((0..now.len()).collect());
        };
        let table = &self.catalog.tables[ti];
        let unique = unique_key(table);
        let keyed = unique.is_some();
        let columns: Vec<usize> = unique.unwrap_or_else(|| (0..table.columns.len()).collect());
        let key = |r: &Row| -> Vec<Datum> { columns.iter().map(|&c| r[c].clone()).collect() };
        // How many rows it found the first time holding each key.
        let mut owed: std::collections::HashMap<Vec<Datum>, usize> = Default::default();
        for f in first {
            *owed.entry(key(f)).or_default() += 1;
        }
        // At REPEATABLE READ a row found that another transaction changed or deleted since the
        // transaction's generation is refused, in PostgreSQL's words, SQLSTATE 40001: one of rows
        // alike is gone where fewer of them stand now than it found.
        if self.levels.repeatable(self.begun.is_some()) {
            let width = table.columns.len();
            let mut newest: std::collections::HashMap<Vec<Datum>, Vec<&Row>> = Default::default();
            for r in now {
                newest.entry(key(r)).or_default().push(r);
            }
            for f in first {
                let k = key(f);
                match newest.get(&k) {
                    Some(rs) if rs.len() >= owed[&k] && rs[0][..width] != f[..width] => {
                        return Err(Error::new(
                            "could not serialize access due to concurrent update",
                        ))
                    }
                    Some(rs) if rs.len() >= owed[&k] => {}
                    _ => {
                        return Err(Error::new(
                            "could not serialize access due to concurrent delete",
                        ))
                    }
                }
            }
        }
        // Rows found that no longer stand alike, as many of each as are gone: where an update
        // replaced them, the keys on the table's order of the rows that replaced them.
        let mut replaced: std::collections::HashMap<Vec<KeyValue>, usize> = Default::default();
        if !keyed {
            let mut standing: std::collections::HashMap<Vec<Datum>, usize> = Default::default();
            for r in now {
                *standing.entry(key(r)).or_default() += 1;
            }
            let mut gone: Vec<(&Row, usize)> = Vec::new();
            let mut met: std::collections::HashSet<Vec<Datum>> = Default::default();
            for f in first {
                let k = key(f);
                let n = owed[&k].saturating_sub(standing.get(&k).copied().unwrap_or(0));
                if n > 0 && met.insert(k) {
                    gone.push((f, n));
                }
            }
            if !gone.is_empty() {
                for to in self.chained_from(ti, &gone)? {
                    *replaced.entry(to).or_default() += 1;
                }
            }
        }
        let order = table
            .orders
            .iter()
            .find(|o| o.table_order)
            .or(table.orders.first());
        let mut kept = Vec::new();
        for (i, r) in now.iter().enumerate() {
            if let Some(n) = owed.get_mut(&key(r)).filter(|n| **n > 0) {
                *n -= 1;
                kept.push(i);
                continue;
            }
            let Some(order) = order.filter(|_| !replaced.is_empty()) else {
                continue;
            };
            if let Some(n) = replaced
                .get_mut(&key_of(table, order, r)?)
                .filter(|n| **n > 0)
            {
                *n -= 1;
                kept.push(i);
            }
        }
        Ok(kept)
    }

    /// Where the statement under way began: what it had staged and the root it read, so that it
    /// can run again from there.
    pub(super) fn mark(&self) -> Mark {
        Mark {
            root: self.root.clone(),
            own: self.own.clone(),
            first_bytes: self.first_bytes.clone(),
            unnamed: self.unnamed.clone(),
            rebased: self.rebased.clone(),
            staged: self.reader.staged_mark(),
            touched: self.touched.clone(),
            pending: self.pending.clone(),
            modes: self.modes.clone(),
        }
    }

    pub(super) fn back_to(&mut self, mark: &Mark) {
        self.root = mark.root.clone();
        self.own = mark.own.clone();
        self.first_bytes = mark.first_bytes.clone();
        self.unnamed = mark.unnamed.clone();
        self.rebased = mark.rebased.clone();
        self.reader.staged_back_to(mark.staged);
        self.touched = mark.touched.clone();
        self.pending = mark.pending.clone();
        self.modes = mark.modes.clone();
    }

    /// What a statement of a transaction reads: the newest root, with the transaction's own changes
    /// in place of the blocks it changed, each still the block it began from, since the
    /// transaction holds its lock. Returns the pages it read.
    pub(super) fn renew_in_transaction(&mut self) -> Result<u64> {
        let before = self.reader.pages_read();
        let root = self.reader.begin_round()?;
        self.in_transaction_on(root)?;
        Ok(self.reader.pages_read() - before)
    }

    /// What a statement of a transaction reads on `root`: the transaction's own changes in place of
    /// the blocks it changed. A transaction that declared reads the catalog as committed with its
    /// own statements declared on it (`declared_on`).
    pub(super) fn in_transaction_on(&mut self, mut root: Root) -> Result<()> {
        if root.turn == self.root.turn {
            return Ok(());
        }
        if self.declared.is_some() {
            self.declared_on(&mut root)?;
        } else if self.catalog_moved(&root) {
            // What it holds of each table by its place follows the table where another's DDL
            // moved it.
            let catalog = Snail::catalog(&self.reader, &root)?;
            self.renumbered(&catalog);
            self.catalog = catalog;
        }
        let mut stage = self.reader.stage();
        for ((ti, oi), blocks) in self.rebased.clone() {
            let table = &self.catalog.tables[ti];
            let order = &table.orders[oi];
            let at = root
                .orders
                .iter()
                .position(|(t, o, _)| *t == table.name && *o == order.name)
                .ok_or_else(|| {
                    Error::new(format!("{}: the root names no {}", table.name, order.name))
                })?;
            let (mut head, entries) =
                crate::write::top_read(table, order, &self.reader.read_extent(root.orders[at].2)?)?;
            let mut top = Node {
                entries,
                height: head.height,
                kids: BTreeMap::new(),
                at: Extent::default(),
            };
            let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
            for b in blocks {
                if let Some(first) = &b.head {
                    head = first.clone();
                }
                if b.child == Extent::default() {
                    top.entries = b.entries;
                    top.height = 1;
                    continue;
                }
                let mut path = Vec::new();
                self.locate(&mut top, &head, &b.first, &mut path)?;
                if entry_at(&top, &path).child != b.child {
                    return Err(Error::new(format!(
                        "{}: a block of {} the transaction holds was written by another writer",
                        table.name, order.name
                    )));
                }
                replaced.insert(path, b.entries);
            }
            settle(
                &mut top,
                &mut Vec::new(),
                &replaced,
                &mut stage,
                &mut Vec::new(),
                &head,
            )?;
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| stage.place(x))?;
            root.orders[at].2 = stage.place(&bytes)?;
        }
        self.root = root;
        Ok(())
    }

    /// Drops what the statement or the transaction staged, the catalog it declared among it, and
    /// lets go of the blocks it locked.
    pub(super) fn abandon(&mut self) -> Result<()> {
        self.rebased.clear();
        self.savepoints.clear();
        self.own.clear();
        self.own_from = None;
        self.first_bytes.clear();
        self.deciding = None;
        self.marking = None;
        self.reader.clear_staged();
        self.touched = Touched::default();
        self.pending = Touched::default();
        self.modes.clear();
        self.seen = None;
        let laid = self.snapshot_catalog.take().is_some();
        if self.declared.take().is_some() || laid {
            self.catalog = Snail::catalog(&self.reader, &self.root)?;
        }
        self.let_go_blocks()?;
        self.serial_end()
    }

    /// Lets go of every block and row it locked, and of its marks on the tables whose rows it
    /// locked or changed.
    pub(super) fn let_go_blocks(&mut self) -> Result<()> {
        for b in std::mem::take(&mut self.locks) {
            self.unlock_byte(b)?;
        }
        for b in std::mem::take(&mut self.row_locks).into_keys() {
            self.unlock_byte(b)?;
        }
        self.let_go_tables()?;
        self.unnamed.clear();
        self.held = false;
        self.comparing.set(false);
        Ok(())
    }

    /// Lets go of one block's lock, no longer marked held first.
    pub(super) fn unlock_byte(&self, b: u64) -> Result<()> {
        if let Some(w) = self.writer.as_ref() {
            lock::mark_held(w.file(), self.slot, b, false)?;
            lock::unlock_block(w.file(), b)?;
        }
        Ok(())
    }

    /// Takes one block's lock and marks it held. Where another writer holds it, `Try` returns false
    /// at once; `Wait` marks that it waits, follows the writers each waiting for a block the next
    /// holds, and waits as long as it takes unless they come back to a block this writer holds: a
    /// ring, which PostgreSQL ends by failing one of its transactions, as this one fails.
    pub(super) fn lock_byte(&self, b: u64, waiting: Waiting) -> Result<bool> {
        self.lock_byte_as(b, false, waiting)
    }

    /// Takes one byte's lock, shared or alone, and marks it held, as `lock_byte` takes a block's.
    pub(super) fn lock_byte_as(&self, b: u64, shared: bool, waiting: Waiting) -> Result<bool> {
        if waiting == Waiting::Wait {
            self.lock_byte_for(b, shared, "a block")?;
            return Ok(true);
        }
        let file = self.writer.as_ref().expect("a snail open to write").file();
        if !lock::lock_as(file, b, shared, false)? {
            return Ok(false);
        }
        lock::mark_held(file, self.slot, b, true)?;
        Ok(true)
    }

    /// Takes one byte's lock, shared or alone, waiting while another writer holds it, as long as it
    /// takes, unless the writers waiting make a ring, refused in words naming `what` it waits for;
    /// marked held once taken.
    pub(super) fn lock_byte_for(&self, b: u64, shared: bool, what: &str) -> Result<()> {
        let file = self.writer.as_ref().expect("a snail open to write").file();
        if !lock::lock_as(file, b, shared, false)? {
            lock::mark_waiting(file, self.slot, b, true)?;
            let (group, me) = self.group.as_ref().expect("a snail open to write");
            let ring = group.ring(*me, b);
            if !matches!(ring, Ok(false)) {
                lock::mark_waiting(file, self.slot, b, false)?;
                ring?;
                return Err(Error::new(format!(
                    "deadlock detected: this writer waits for {what} held by a writer that waits, \
                     in a ring, for a lock this writer holds"
                )));
            }
            let taken = super::settings::wait_for_lock(|w| lock::lock_as(file, b, shared, w));
            lock::mark_waiting(file, self.slot, b, false)?;
            taken?;
        }
        lock::mark_held(file, self.slot, b, true)
    }

    pub(super) fn written(&self) -> u64 {
        self.writer.as_ref().map_or(0, Writer::pages_written)
    }

    /// Commits what the statement, or the transaction, staged: each block it wrote spliced into the
    /// newest directory in place of the block it began from, its staged pages written into the file,
    /// the directory pages above them afresh, then the root.
    pub(super) fn commit(&mut self) -> Result<()> {
        let mut w = self.writer.take().expect("a snail open to write");
        let turn = lock::take_root_turn(w.file());
        let committed = turn
            .and_then(|_| self.serial_tested())
            .and_then(|_| self.commit_with(&mut w));
        let given = lock::give_root_turn(w.file());
        self.writer = Some(w);
        match committed {
            Ok(root) => self.root = root,
            Err(e) => {
                // A commit that fails lets go of what it held, as one that holds.
                self.abandon()?;
                return Err(e);
            }
        }
        given?;
        self.abandon()
    }

    /// The commit, at the root's turn: it begins from the newest root.
    pub(super) fn commit_with(&mut self, w: &mut Writer) -> Result<Root> {
        // A transaction's root carries its host's tag; a take lands at once.
        let advance = self.is_take();
        let mut root = self.commit_pages(w, &[])?;
        root.host = match w.host_tag() {
            Some(_) if advance => Some(crate::carry::AT_ONCE),
            tag => tag,
        };
        // The root goes into the copy that does not hold the root it begins from.
        w.commit_into(&mut root, 1 - self.reader.root_copy())?;
        Ok(root)
    }

    /// Whether the root written now is a take, no commit: a sequence's move, OIDs taken, or the
    /// transactions a rollback doomed.
    fn is_take(&self) -> bool {
        self.advancing.is_some() || self.taking.is_some() || self.ending.is_some()
    }

    /// The commit but its root, at the root's turn: every page written, and the root that names
    /// them, less the decisions across snails `landed` names by their generations, whose pages it
    /// frees. A snail whose other copy of the root waits on a transaction across snails takes no
    /// commit until a round across them finishes it or lets it go.
    pub(super) fn commit_pages(&mut self, w: &mut Writer, landed: &[u64]) -> Result<Root> {
        let mut root = self.reader.peek_root()?;
        if let Some(p) = self.reader.pending_copy().and_then(|r| r.pending) {
            return Err(Error::new(format!(
                "this snail holds a root written for a transaction across snails, decided by the \
                 snail that holds {} at its generation {}: open the two together to write it",
                p.decider, p.at
            )));
        }
        // A snail written through a host is written only through it; and a host's transaction
        // commits it once, besides the advances of its sequences and the OIDs it takes.
        if !self.reader.judged() && (root.host.is_some() || self.reader.waiting_on().is_some()) {
            return Err(Error::new(
                "this snail is written through its host, which decides which of its roots is \
                 taken: write it through its host",
            ));
        }
        let at_once = self.advancing.is_some() || self.taking.is_some();
        if let Some(t) = w.host_tag().filter(|&t| t != crate::carry::AT_ONCE) {
            let lies = root.host == Some(t) || self.reader.waiting_on() == Some(t);
            if lies && !at_once {
                return Err(Error::new(format!(
                    "the host's transaction {t} commits this snail once, and its commit lies on \
                     the root already"
                )));
            }
        }
        // A snail marked a replica is written by the applier presenting its source alone, but for
        // the commit that sets or clears the mark, at the root's turn; a sequence's advance lands
        // ahead of the transaction that sets it.
        if self.marking.is_none() {
            self.writes_marked(&root)?;
        }
        if !at_once {
            if let Some(m) = self.marking.take() {
                if let (Some(stands), Some(new)) = (&root.replica, &m) {
                    if stands != new {
                        return Err(Error::new(format!(
                            "this snail is marked a replica of '{stands}': clear the mark before \
                             marking it a replica of '{new}'"
                        )));
                    }
                }
                root.replica = m;
            }
        }
        root.host = None;
        w.begin_commit(&root);
        let turn = root.turn + 1;
        let take = self.is_take();
        // A commit carries the pages the takes since the last wrote.
        if !take && root.owed.bytes > 0 && w.is_carried() {
            let mut pages = Vec::new();
            for r in decode_runs(&self.reader.read_extent(root.owed)?)? {
                let bytes = self.reader.read(r.start, r.pages)?;
                for (i, page) in bytes.chunks(crate::cut::PAGE).enumerate() {
                    pages.push((r.start + i as u64, page.to_vec()));
                }
            }
            w.carries_also(pages);
        }
        // The free map: its runs whose tag no older round still runs under are free now, and the
        // commit takes pages from the free runs first.
        let (mut runs, mut spent) = if root.free.bytes > 0 {
            decode_free(&self.reader.read_extent(root.free)?)?
        } else {
            (Vec::new(), Vec::new())
        };
        let mut maps = MapPages::new(true);
        let mut gone: Vec<Extent> = Vec::new();
        if root.free.bytes > 0 {
            gone.push(root.free);
        }
        for d in &root.decided {
            if landed.contains(&d.at) && d.carried.bytes > 0 {
                gone.push(d.carried);
            }
        }
        root.decided.retain(|d| !landed.contains(&d.at));
        let mut tags: Vec<u64> = runs.iter().map(|r| r.tag).filter(|&t| t > 0).collect();
        tags.sort_unstable();
        tags.dedup();
        // A run a take freed waits for a commit.
        let committed = root.turn - root.takes;
        let mut freed = 0;
        for t in tags {
            if t > committed || lock::older_held(self.reader.file(), t)? {
                break;
            }
            freed = t;
        }
        for r in &mut runs {
            if r.tag > 0 && r.tag <= freed {
                r.tag = 0;
            }
        }
        let free: Vec<(u64, u64)> = runs
            .iter()
            .filter(|r| r.tag == 0)
            .map(|r| (r.start, r.pages))
            .collect();
        for &(start, pages) in &free {
            self.reader.forget(start, pages);
        }
        w.use_free(free);
        let waiting: Vec<Run> = runs.into_iter().filter(|r| r.tag > 0).collect();
        // The catalog declared: its statements declared again where another writer's commit moved
        // the catalog since, then written afresh, each table it touches given the orders it
        // declares, those it made empty.
        self.replayed_on(&root)?;
        // An order made on a table that rows another writer committed since now fill is refused,
        // as one made on a table that holds rows is.
        if let Some(d) = self.declared.as_ref() {
            for (t, _) in &d.made {
                for (a, b, top) in &root.orders {
                    if a != t || d.taken_out.contains(&(a.clone(), b.clone())) {
                        continue;
                    }
                    let (_, entries) = decode_top(&self.reader.read_extent(*top)?)?;
                    if entries.iter().any(|e| e.rows > 0) {
                        return Err(Error::new(format!(
                            "{t}: it holds rows, so its columns and orders stay as written"
                        )));
                    }
                }
            }
        }
        self.oids_at_commit(&mut root, w, &mut gone)?;
        // OIDs a statement took from the counter, at the root's turn.
        if let Some(next) = self.taking.take() {
            root.next_oid = next;
        }
        if let Some(d) = self.declared.take() {
            self.reader
                .put_catalog(&mut root, d.ddl.as_bytes(), turn, w, &mut gone)?;
            root.monetary = crate::file::monetary_of(&self.catalog, &root.monetary);
            if !d.resets.is_empty() && root.sequences.bytes > 0 {
                let mut states = self.reader.sequence_states(&root)?;
                states.retain(|s| !d.resets.contains(&s.0));
                self.reader
                    .put_sequences(&mut root, &states, turn, w, &mut gone)?;
            }
            let touched: Vec<&String> = d.made.iter().map(|(t, _)| t).collect();
            let mut orders = Vec::new();
            for (t, o, e) in std::mem::take(&mut root.orders) {
                let made = d.made.contains(&(t.clone(), o.clone()));
                let declared = self
                    .catalog
                    .table(&t)
                    .is_some_and(|x| x.orders.iter().any(|y| y.name == o));
                let taken_out = d.taken_out.contains(&(t.clone(), o.clone()));
                if (touched.contains(&&t) && (made || !declared)) || taken_out {
                    gone.push(e);
                } else {
                    orders.push((t, o, e));
                }
            }
            for (tn, on) in &d.made {
                if let Some(t) = self.catalog.table(tn) {
                    if let Some(o) = t.orders.iter().find(|x| x.name == *on) {
                        let top = w.place(&encode_top(&empty_head(t, o), &[]))?;
                        orders.push((tn.clone(), on.clone(), top));
                    }
                }
            }
            root.orders = orders;
        }
        // The sequences a statement advanced, at the root's turn, each where it stands now.
        if let Some(states) = self.advancing.take() {
            self.reader
                .put_sequences(&mut root, &states, turn, w, &mut gone)?;
        }
        // A transaction across snails this commit decides: the roots it writes into the others,
        // carried.
        if let Some((roots, bytes)) = self.deciding.take() {
            let carried = w.place(&bytes)?;
            root.decided.push(crate::file::Decision {
                at: turn,
                roots,
                carried,
            });
        }
        // The serializable transactions' records, and what the host's monitor is told of its cuts.
        self.tell_cuts()?;
        self.records_at_commit(w, &mut root, turn, &mut gone)?;
        let rebased = std::mem::take(&mut self.rebased);
        for ((ti, oi), blocks) in rebased {
            let table = &self.catalog.tables[ti];
            let order = &table.orders[oi];
            let at = root
                .orders
                .iter()
                .position(|(t, o, _)| *t == table.name && *o == order.name)
                .ok_or_else(|| {
                    Error::new(format!("{}: the root names no {}", table.name, order.name))
                })?;
            let (mut head, entries) =
                crate::write::top_read(table, order, &self.reader.read_extent(root.orders[at].2)?)?;
            let mut top = Node {
                entries,
                height: head.height,
                kids: BTreeMap::new(),
                at: Extent::default(),
            };
            let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
            for b in blocks {
                if let Some(first) = &b.head {
                    head = first.clone();
                }
                let (entries, held): (Vec<Entry>, Vec<Vec<Extent>>) = b
                    .entries
                    .into_iter()
                    .map(|e| self.relocate(w, &head, e, &mut maps))
                    .collect::<Result<Vec<_>>>()?
                    .into_iter()
                    .unzip();
                if b.child == Extent::default() {
                    if !top.entries.is_empty() {
                        return Err(Error::new(format!(
                            "{}: {} was written since by another writer",
                            table.name, order.name
                        )));
                    }
                    top.entries = entries;
                    top.height = 1;
                    continue;
                }
                let mut path = Vec::new();
                self.locate(&mut top, &head, &b.first, &mut path)?;
                let was = entry_at(&top, &path).clone();
                if was.child != b.child {
                    return Err(Error::new(format!(
                        "{}: a block of {} the statement wrote was written since by another writer",
                        table.name, order.name
                    )));
                }
                // The block's pages the entries standing for it now no longer name.
                let kept: std::collections::HashSet<u64> =
                    held.into_iter().flatten().map(|e| e.start).collect();
                gone.extend(
                    self.extents(&head, &was)?
                        .into_iter()
                        .filter(|e| !kept.contains(&e.start)),
                );
                // A page of packed maps is freed once the last of its maps is replaced.
                if let Some((page, n)) = self.shared_map(&was)? {
                    if !entries.iter().any(|e| e.child == was.child) {
                        let i = match spent.iter().position(|&(p, _)| p == page) {
                            Some(i) => i,
                            None => {
                                spent.push((page, 0));
                                spent.len() - 1
                            }
                        };
                        spent[i].1 += 1;
                        if spent[i].1 == n {
                            spent.remove(i);
                            gone.push(Extent {
                                start: page,
                                bytes: crate::cut::PAGE as u64,
                            });
                        }
                    }
                }
                replaced.insert(path, entries);
            }
            settle(&mut top, &mut Vec::new(), &replaced, w, &mut gone, &head)?;
            gone.push(root.orders[at].2);
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            root.orders[at].2 = w.place(&bytes)?;
        }
        maps.finish(w)?;
        self.reader.settle(&mut root, turn, w, &mut gone)?;
        // What the takes since the last commit wrote, for the next commit to carry.
        if root.owed.bytes > 0 {
            gone.push(root.owed);
        }
        if take {
            let mut owed = if root.owed.bytes > 0 {
                decode_runs(&self.reader.read_extent(root.owed)?)?
            } else {
                Vec::new()
            };
            owed.extend(w.runs_written().into_iter().map(|(start, pages)| Run {
                start,
                pages,
                tag: 0,
            }));
            root.owed = match owed.is_empty() {
                true => Extent::default(),
                false => w.place(&encode_runs(&owed))?,
            };
            root.takes += 1;
        } else {
            root.owed = Extent::default();
            root.takes = 0;
            root.generation += 1;
        }
        spent.sort_unstable();
        // The free map as it stands now: the runs left free, those still waiting, and the pages
        // this generation stops naming, tagged with it.
        let mut map: Vec<Run> = w
            .left_free()
            .into_iter()
            .map(|(start, pages)| Run {
                start,
                pages,
                tag: 0,
            })
            .chain(waiting)
            .chain(gone.iter().map(|e| Run {
                start: e.start,
                pages: e.pages().max(1),
                tag: turn,
            }))
            .collect();
        map.sort_by_key(|r| r.start);
        let mut runs: Vec<Run> = Vec::with_capacity(map.len());
        for r in map {
            match runs.last_mut() {
                Some(l) if l.tag == r.tag && l.start + l.pages == r.start => l.pages += r.pages,
                _ => runs.push(r),
            }
        }
        root.free = if runs.is_empty() && spent.is_empty() {
            Extent::default()
        } else {
            w.place_end(&encode_free(&runs, &spent))?
        };
        for (start, pages) in w.taken() {
            self.reused += pages;
            root.reused.push((turn, start, pages));
        }
        while root.reused.len() > REUSED_KEPT {
            let (g, _, _) = root.reused.remove(0);
            root.reused_from = root.reused_from.max(g);
        }
        root.turn = turn;
        Ok(root)
    }

    /// Every page an entry's block holds alone: its page map's run where the map lies alone in it, and
    /// each column's dictionary and pages.
    fn extents(&self, head: &Head, e: &Entry) -> Result<Vec<Extent>> {
        let (run, at) = e.map_run();
        let buf = self.reader.read_extent(run)?;
        let map = PageMap::decode(&buf[at..], &head.level_text)?;
        let mut out = Vec::new();
        if maps_held(&buf, at) == 1 {
            out.push(run);
        }
        for c in &map.columns {
            out.extend(c.dictionary);
            out.extend(c.pages.iter().map(|p| p.at));
        }
        Ok(out)
    }

    /// The page a block's page map shares with others, and how many maps it holds; none where the
    /// map lies alone in its run.
    fn shared_map(&self, e: &Entry) -> Result<Option<(u64, u32)>> {
        let (run, at) = e.map_run();
        if at == 0 {
            return Ok(None);
        }
        let first = self.reader.read_extent(Extent {
            start: run.start,
            bytes: MAP_HEAD as u64,
        })?;
        Ok(match maps_held(&first, at) {
            1 => None,
            n => Some((run.start, n)),
        })
    }

    /// An entry whose pages are staged, its pages written into the file and its page map with them:
    /// packed with the other maps the commit writes, where the snail packs its maps. Returns the
    /// entry, and the pages its block holds alone, as `extents` gives them.
    fn relocate(
        &self,
        w: &mut Writer,
        head: &Head,
        mut e: Entry,
        maps: &mut MapPages,
    ) -> Result<(Entry, Vec<Extent>)> {
        if e.child.start < STAGED {
            let held = self.extents(head, &e)?;
            return Ok((e, held));
        }
        let mut map = read_map(&self.reader, &e, &head.level_text)?;
        let whole = map.columns.iter().all(|c| {
            c.dictionary.is_none_or(|d| d.start >= STAGED)
                && c.pages.iter().all(|p| p.at.start >= STAGED)
        });
        if whole {
            // A block written whole lies in one run, in one file, its page map first and its columns
            // after it, as a write lays a block out, the map alone at the run's start.
            let bytes = map.encode().len();
            let map_pages = bytes.div_ceil(crate::cut::PAGE).max(1) as u64;
            let data: u64 = map
                .columns
                .iter()
                .flat_map(|c| c.dictionary.into_iter().chain(c.pages.iter().map(|p| p.at)))
                .map(|x| x.pages().max(1))
                .sum();
            let start = w.reserve(map_pages + data);
            let mut at = start + map_pages;
            let mut copy = |x: Extent| -> Result<Extent> {
                w.write_at(at, &self.reader.read_extent(x)?)?;
                let moved = Extent {
                    start: at,
                    bytes: x.bytes,
                };
                at += x.pages().max(1);
                Ok(moved)
            };
            for c in &mut map.columns {
                if let Some(d) = c.dictionary {
                    c.dictionary = Some(copy(d)?);
                }
                for p in &mut c.pages {
                    p.at = copy(p.at)?;
                }
            }
            let encoded = map.encode();
            if encoded.len() != bytes {
                return Err(Error::new(
                    "a page map changed its size when its block moved",
                ));
            }
            w.write_at(start, &encoded)?;
            e.child = map_child(start, 0, bytes);
            let held = std::iter::once(e.map_run().0)
                .chain(
                    map.columns
                        .iter()
                        .flat_map(|c| c.dictionary.into_iter().chain(c.pages.iter().map(|p| p.at))),
                )
                .collect();
            return Ok((e, held));
        }
        let mut copy = |x: Extent| -> Result<Extent> {
            if x.start < STAGED {
                Ok(x)
            } else {
                w.place(&self.reader.read_extent(x)?)
            }
        };
        for c in &mut map.columns {
            if let Some(d) = c.dictionary {
                c.dictionary = Some(copy(d)?);
            }
            for p in &mut c.pages {
                p.at = copy(p.at)?;
            }
        }
        let encoded = map.encode();
        let (child, _) = maps.place(w, encoded.len(), 0, false)?;
        maps.write(w, child, &encoded)?;
        e.child = child;
        let held = map
            .columns
            .iter()
            .flat_map(|c| c.dictionary.into_iter().chain(c.pages.iter().map(|p| p.at)))
            .chain((e.map_run().1 == 0).then(|| e.map_run().0))
            .collect();
        Ok((e, held))
    }

    /// Notes that a block of an order is replaced by `new`: either a block the transaction began from,
    /// or one it staged itself, found among those that stand for a block it began from.
    fn rebase(&mut self, key: (usize, usize), old: &Entry, new: &[Entry]) -> Result<()> {
        let log = self.rebased.entry(key).or_default();
        if old.child.start < STAGED {
            log.push(Rebased {
                first: old.first.clone(),
                child: old.child,
                entries: new.to_vec(),
                head: None,
            });
            return Ok(());
        }
        for r in log.iter_mut() {
            if let Some(i) = r.entries.iter().position(|e| e.child == old.child) {
                r.entries.splice(i..i + 1, new.iter().cloned());
                return Ok(());
            }
        }
        Err(Error::new(
            "a block staged is none of the blocks the transaction wrote",
        ))
    }

    #[doc(hidden)]
    pub fn stop_at(&mut self, stop: Option<crate::file::Stop>) {
        if let Some(w) = self.writer.as_mut() {
            w.stop_at(stop);
        }
    }

    /// Applies one statement's changes to every order of a table. Returns the blocks rewritten in
    /// order, and the orders written.
    pub(super) fn apply(&mut self, ti: usize, changes: Vec<Change>) -> Result<(u64, u64)> {
        let table = self.catalog.tables[ti].clone();
        for c in &changes {
            match c {
                Change::Insert(r) | Change::Replace(_, r) => {
                    check(&table, r)?;
                }
                Change::Delete(_) => {}
            }
        }
        // The rows it deletes or replaces, named before any block is locked.
        let named: Vec<(&Row, bool)> = changes
            .iter()
            .filter_map(|c| match c {
                Change::Replace(o, n) => Some((o, super::locking::key_changed(&table, o, n))),
                Change::Delete(r) => Some((r, true)),
                Change::Insert(_) => None,
            })
            .collect();
        // A writer of the table holds its lock in ROW EXCLUSIVE, as PostgreSQL's does; where it
        // waited for another writer's DDL, whose commit moved the catalog, the statement is read
        // again on the catalog as committed.
        if self.lock_table(ti, super::tablelock::Mode::RowExclusive, false)?
            && self.catalog_moved(&self.reader.peek_root()?)
        {
            self.stale = true;
            return Ok((0, 0));
        }
        self.name_rows(ti, &named)?;
        // A transaction of one snapshot writes the version it reads; its commit writes the newest.
        if self.held {
            return self.apply_beside(ti, &table, changes);
        }
        let mut stage = self.reader.stage();
        let applied: Vec<u64> = (0..table.orders.len())
            .map(|oi| self.apply_order(&mut stage, ti, &table, oi, &changes))
            .collect::<Result<Vec<Option<u64>>>>()?
            .into_iter()
            .flatten()
            .collect();
        let (rewritten, orders) = self.apply_to_strands(&table, &changes)?;
        if !self.stale {
            self.touch(ti, &changes);
            self.touch_exclusions(ti, &changes);
        }
        Ok((
            applied.iter().sum::<u64>() + rewritten,
            applied.len() as u64 + orders,
        ))
    }

    /// What a statement's changes come to on one order: for each row, the row taken from it and the
    /// row written into it, keys and values as the order holds them. A partial strand takes the rows
    /// its predicate admits: a row replaced by one it does not admit leaves it, and one it did not
    /// admit comes in when its replacement passes.
    pub(super) fn effects_on(
        &self,
        table: &Table,
        order: &Order,
        changes: &[Change],
    ) -> Result<Vec<Effect>> {
        let held =
            |r: &Row| -> Vec<Datum> { order.columns.iter().map(|&c| r[c].clone()).collect() };
        let predicate = super::partial::Predicate::of(&self.catalog, table, order)?;
        let admits = |r: &Row| predicate.as_ref().is_none_or(|p| p.admits(table, r));
        let routed: Vec<Change>;
        let changes = if predicate.is_some() {
            routed = changes
                .iter()
                .filter_map(|c| match c {
                    Change::Insert(r) | Change::Delete(r) if !admits(r) => None,
                    Change::Replace(o, n) => match (admits(o), admits(n)) {
                        (true, true) => Some(c.clone()),
                        (true, false) => Some(Change::Delete(o.clone())),
                        (false, true) => Some(Change::Insert(n.clone())),
                        (false, false) => None,
                    },
                    c => Some(c.clone()),
                })
                .collect();
            routed.as_slice()
        } else {
            changes
        };
        // A row replaced in place, as 18.6's UPDATE is HOT where its page has room: its key on the
        // table's order the same, and every strand holding the same values of it.
        let in_place = |o: &Row, n: &Row| -> Result<bool> {
            for x in &table.orders {
                let alike = if x.table_order {
                    key_of(table, x, o)? == key_of(table, x, n)?
                } else {
                    let held_x = |r: &Row| -> Vec<Datum> {
                        x.columns.iter().map(|&c| r[c].clone()).collect()
                    };
                    super::image::held_alike(&held_x(o), &held_x(n))
                };
                if !alike {
                    return Ok(false);
                }
            }
            Ok(true)
        };
        let mut effects = Vec::new();
        for c in changes {
            match c {
                Change::Insert(r) => effects.push(Effect {
                    key: key_of(table, order, r)?,
                    values: held(r),
                    delete: false,
                    chain: None,
                    lands: true,
                }),
                Change::Delete(r) => effects.push(Effect {
                    key: key_of(table, order, r)?,
                    values: held(r),
                    delete: true,
                    chain: None,
                    lands: false,
                }),
                Change::Replace(o, n) => {
                    if super::image::held_alike(&held(o), &held(n)) {
                        continue;
                    }
                    let to = key_of(table, order, n)?;
                    effects.push(Effect {
                        key: key_of(table, order, o)?,
                        values: held(o),
                        delete: true,
                        chain: Some(to.clone()),
                        lands: false,
                    });
                    effects.push(Effect {
                        key: to,
                        values: held(n),
                        delete: false,
                        chain: None,
                        lands: !order.table_order || !in_place(o, n)?,
                    });
                }
            }
        }
        Ok(effects)
    }

    /// What a write of one order marks for a serializable round: each block it changes, by its byte;
    /// on the table's own order, each row's key it changes; and each block a key lands in, a block the
    /// transaction already staged by the block it began from.
    fn written_of(
        &self,
        ti: usize,
        oi: usize,
        order: &Order,
        blocks: &[(Entry, bool)],
        effects: &[&Effect],
    ) -> super::serial::Written {
        let key = self.lock_key(ti);
        let byte = |e: &Entry| lock::block_byte_of(key, oi, &e.first);
        let begun = |e: &Entry| {
            self.rebased
                .get(&(ti, oi))
                .and_then(|rs| {
                    rs.iter()
                        .find(|r| r.entries.iter().any(|x| x.child.start == e.child.start))
                })
                .map_or_else(|| byte(e), |r| lock::block_byte_of(key, oi, &r.first))
        };
        super::serial::Written {
            blocks: blocks
                .iter()
                .filter(|(e, _)| e.child.start < STAGED)
                .map(|(e, _)| byte(e))
                .collect(),
            rows: match order.table_order {
                true => effects
                    .iter()
                    .map(|e| lock::row_byte(key, oi, &e.key))
                    .collect(),
                false => Vec::new(),
            },
            landed: blocks
                .iter()
                .filter(|(_, lands)| *lands)
                .map(|(e, _)| match e.child.start < STAGED {
                    true => byte(e),
                    false => begun(e),
                })
                .collect(),
        }
    }

    /// One order's part of a statement: its effects routed to their blocks, each block written
    /// afresh, then the directory above them up to a new top. Returns the blocks rewritten in order,
    /// or none where the order holds no column the statement changed.
    pub(super) fn apply_order(
        &mut self,
        w: &mut dyn Place,
        ti: usize,
        table: &Table,
        oi: usize,
        changes: &[Change],
    ) -> Result<Option<u64>> {
        if self.stale {
            return Ok(None);
        }
        let order = &table.orders[oi];
        let effects = self.effects_on(table, order, changes)?;
        if effects.is_empty() {
            return Ok(None);
        }
        let at = self
            .root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
            .ok_or_else(|| {
                Error::new(format!("{}: the root names no {}", table.name, order.name))
            })?;
        let (mut head, entries) = crate::write::top_read(
            table,
            order,
            &self.reader.read_extent(self.root.orders[at].2)?,
        )?;
        let mut top = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        let mut pruned = 0;
        let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
        if top.entries.is_empty() {
            if effects.iter().any(|e| e.delete) {
                return Err(Error::new(format!("{} holds no row to delete", order.name)));
            }
            // The order's first rows: its lock taken, as a block's is, and the order still empty.
            let b = lock::block_byte_of(self.lock_key(ti), oi, &[]);
            if !self.locks.contains(&b) {
                self.lock_byte(b, Waiting::Wait)?;
                self.locks.insert(b);
                if !self.empty_in_newest(table, order)? {
                    self.stale = true;
                    return Ok(None);
                }
            }
            let mut written = super::serial::Written::blocks(vec![b]);
            written.landed.push(b);
            if order.table_order {
                let key = self.lock_key(ti);
                written.rows = effects
                    .iter()
                    .map(|e| lock::row_byte(key, oi, &e.key))
                    .collect();
            }
            self.mark_wrote(ti, oi, &written)?;
            let rows: Vec<Vec<Datum>> = effects.into_iter().map(|e| e.values).collect();
            let (entries, first) = self.written_in_order(w, table, order, rows)?;
            top.entries = entries;
            top.height = 1;
            head = first;
            self.rebased.entry((ti, oi)).or_default().push(Rebased {
                first: Vec::new(),
                child: Extent::default(),
                entries: top.entries.clone(),
                head: Some(head.clone()),
            });
        } else {
            let mut by_block: BTreeMap<Vec<usize>, Vec<Effect>> = BTreeMap::new();
            for e in effects {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &e.key, &mut path)?;
                // A row written is checked against the block after its own, where glibc orders it.
                if let (false, Some(next)) = (e.delete, following(&top, &path)) {
                    crate::write::agreed_beside(table, order, &e.key, next)?;
                }
                by_block.entry(path).or_default().push(e);
            }
            let blocks: Vec<Entry> = by_block.keys().map(|p| entry_at(&top, p).clone()).collect();
            let landing: Vec<(Entry, bool)> = by_block
                .iter()
                .map(|(p, es)| (entry_at(&top, p).clone(), es.iter().any(|e| e.lands)))
                .collect();
            let all: Vec<&Effect> = by_block.values().flatten().collect();
            let written = self.written_of(ti, oi, order, &landing, &all);
            if !self.lock_blocks(ti, oi, blocks, Waiting::Wait)? {
                self.stale = true;
                return Ok(None);
            }
            self.mark_wrote(ti, oi, &written)?;
            for (path, effects) in by_block {
                let entry = entry_at(&top, &path).clone();
                let (new, rewrote) = self.write_block_afresh(
                    w,
                    table,
                    order,
                    &head,
                    &entry,
                    effects,
                    Writing::Effects,
                )?;
                pruned += rewrote as u64;
                self.rebase((ti, oi), &entry, &new)?;
                replaced.insert(path, new);
            }
        }
        let mut path = Vec::new();
        settle(&mut top, &mut path, &replaced, w, &mut Vec::new(), &head)?;
        let (bytes, _) = build_from(head, top.entries, top.height, &mut |b| w.place(b))?;
        let extent = w.place(&bytes)?;
        self.root.orders[at].2 = extent;
        Ok(Some(pruned))
    }

    /// Locks the blocks a statement changes before it reads them: each block the transaction began
    /// from, by its table, order and first key, in the order of their locks; a block the transaction
    /// staged lies under the lock of the block it began from. Returns false where a lock was not
    /// taken, under `Waiting::Try`, or where a block was written since the root the round reads.
    pub(super) fn lock_blocks(
        &mut self,
        ti: usize,
        oi: usize,
        blocks: Vec<Entry>,
        waiting: Waiting,
    ) -> Result<bool> {
        let key = self.lock_key(ti);
        let mut want: Vec<(u64, Entry)> = blocks
            .into_iter()
            .filter(|e| e.child.start < STAGED)
            .map(|e| (lock::block_byte_of(key, oi, &e.first), e))
            .collect();
        want.sort_by_key(|x| x.0);
        want.dedup_by_key(|x| x.0);
        let mut got = Vec::new();
        for (b, _) in &want {
            if self.locks.contains(b) || got.contains(b) {
                continue;
            }
            if !self.lock_byte(*b, waiting)? {
                for &g in &got {
                    self.unlock_byte(g)?;
                    self.locks.remove(&g);
                }
                return Ok(false);
            }
            got.push(*b);
            self.locks.insert(*b);
        }
        self.locks.extend(got);
        // Each block, under its lock, is the block the newest root names at its first key.
        let blocks: Vec<Entry> = want.into_iter().map(|(_, e)| e).collect();
        self.newest_names(ti, oi, &blocks)
    }

    /// Whether the newest root names each block the round reads, by its first key: a block the
    /// transaction staged lies under the lock of the block it began from, and is its own. An order
    /// the newest root does not name yet, one the transaction declared, holds only blocks of its own.
    pub(super) fn newest_names(&self, ti: usize, oi: usize, blocks: &[Entry]) -> Result<bool> {
        let blocks: Vec<&Entry> = blocks.iter().filter(|e| e.child.start < STAGED).collect();
        let root = self.reader.peek_root()?;
        let table = &self.catalog.tables[ti];
        let order = &table.orders[oi];
        let Some(at) = root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
        else {
            return Ok(blocks.is_empty());
        };
        let (head, entries) =
            crate::write::top_read(table, order, &self.reader.read_extent(root.orders[at].2)?)?;
        if entries.is_empty() && !blocks.is_empty() {
            return Ok(false);
        }
        let mut top = Node {
            entries,
            height: head.height,
            kids: BTreeMap::new(),
            at: Extent::default(),
        };
        for e in blocks {
            let mut path = Vec::new();
            self.locate(&mut top, &head, &e.first, &mut path)?;
            if entry_at(&top, &path).child != e.child {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Whether an order holds no row in the newest root, or is one the newest root does not name yet.
    pub(super) fn empty_in_newest(&self, table: &Table, order: &Order) -> Result<bool> {
        let root = self.reader.peek_root()?;
        match root
            .orders
            .iter()
            .find(|(t, o, _)| *t == table.name && *o == order.name)
        {
            Some((_, _, top)) => Ok(decode_top(&self.reader.read_extent(*top)?)?.1.is_empty()),
            None => Ok(true),
        }
    }

    /// Reads down the directory to the block a key belongs with, keeping the pages read.
    pub(super) fn locate(
        &self,
        node: &mut Node,
        head: &Head,
        key: &[KeyValue],
        path: &mut Vec<usize>,
    ) -> Result<()> {
        let i = pick(&node.entries, key);
        path.push(i);
        if node.height > 1 {
            if !node.kids.contains_key(&i) {
                let entries = decode_page(
                    &self.reader.read_extent(node.entries[i].child)?,
                    head,
                    node.entries[i].bound(),
                )?;
                node.kids.insert(
                    i,
                    Node {
                        entries,
                        height: node.height - 1,
                        kids: BTreeMap::new(),
                        at: node.entries[i].child,
                    },
                );
            }
            self.locate(node.kids.get_mut(&i).unwrap(), head, key, path)?;
        }
        Ok(())
    }

    /// What a round found wasted, as it ends: in a transaction, pruned within it; outside one, kept
    /// for the writer's next commit that writes, the round committing nothing. Returns the blocks
    /// pruned and the pages written.
    pub(super) fn round_wasted(&mut self, wasted: Vec<Wasted>) -> Result<(u64, u64)> {
        if self.begun.is_some() {
            return self.prune(wasted);
        }
        // A snail marked a replica is pruned by its applier's reads alone.
        if self.writes_marked(&self.root).is_err() {
            return Ok((0, 0));
        }
        let named = self.root.catalog_named();
        let mut kept = match self.unpruned.take() {
            Some((at, kept)) if at == named => kept,
            _ => Vec::new(),
        };
        for b in wasted {
            let known = kept
                .iter()
                .any(|k| k.table == b.table && k.order == b.order && k.child == b.child);
            if !known {
                kept.push(b);
            }
        }
        self.unpruned = Some((named, kept));
        Ok((0, 0))
    }

    /// Stages into the commit of a statement or a transaction that wrote the blocks rounds outside a
    /// transaction found wasted since, pruned as a transaction's prune stages them: one it wrote
    /// itself as it stands staged, one cut as it was written left so; one another writer has
    /// written since, or holds, left as it is. The list is let go either way; it is kept only for
    /// the catalog it was read under. Returns the blocks pruned.
    pub(super) fn prune_unpruned(&mut self) -> Result<u64> {
        let Some((named, wasted)) = self.unpruned.take() else {
            return Ok(0);
        };
        if named != self.root.catalog_named()
            || self.declared.is_some()
            || self.held
            || self.writes_marked(&self.root).is_err()
        {
            return Ok(0);
        }
        let wasted = wasted
            .into_iter()
            .map(|mut b| {
                let staged = self
                    .rebased
                    .get(&(b.table, b.order))
                    .and_then(|log| log.iter().find(|r| r.child == b.child));
                if let Some([e]) = staged.map(|r| r.entries.as_slice()) {
                    b.first = e.first.clone();
                    b.child = e.child;
                }
                b
            })
            .collect();
        let mut stage = self.reader.stage();
        self.prune_with(&mut stage, wasted)
    }

    /// Prunes the blocks a round found a page or more of waste in: each rewritten in order, its dead
    /// rows dropped, and cut again; then committed as a write is, or within the transaction begun.
    /// Returns the blocks pruned and the pages written.
    pub(super) fn prune(&mut self, wasted: Vec<Wasted>) -> Result<(u64, u64)> {
        // A snail marked a replica is pruned by its applier's reads alone; the version a transaction of
        // one snapshot reads goes with it unpruned.
        if self.writes_marked(&self.root).is_err() || self.held {
            return Ok((0, 0));
        }
        let written = self.written();
        let mut stage = self.reader.stage();
        let pruned = self.prune_with(&mut stage, wasted)?;
        match self.begun.as_mut() {
            Some((_, any)) => *any |= pruned > 0,
            None if pruned > 0 => self.commit()?,
            None => self.abandon()?,
        }
        Ok((pruned, self.written() - written))
    }

    fn prune_with(&mut self, w: &mut dyn Place, wasted: Vec<Wasted>) -> Result<u64> {
        let mut by_order: BTreeMap<(usize, usize), Vec<Wasted>> = BTreeMap::new();
        for b in wasted {
            by_order.entry((b.table, b.order)).or_default().push(b);
        }
        let mut pruned = 0;
        for ((ti, oi), blocks) in by_order {
            let table = self.catalog.tables[ti].clone();
            let order = &table.orders[oi];
            let at = self
                .root
                .orders
                .iter()
                .position(|(t, o, _)| *t == table.name && *o == order.name)
                .expect("a block read lies in an order the root names");
            let (head, entries) = crate::write::top_read(
                &table,
                order,
                &self.reader.read_extent(self.root.orders[at].2)?,
            )?;
            let mut top = Node {
                entries,
                height: head.height,
                kids: BTreeMap::new(),
                at: Extent::default(),
            };
            let mut replaced: BTreeMap<Vec<usize>, Vec<Entry>> = BTreeMap::new();
            for b in blocks {
                let mut path = Vec::new();
                self.locate(&mut top, &head, &b.first, &mut path)?;
                let entry = entry_at(&top, &path).clone();
                if entry.child != b.child || replaced.contains_key(&path) {
                    continue;
                }
                // A block another writer has locked, or has written since, is read as it is; and so, in a
                // transaction, is one the transaction does not hold, so that its read holds no writer
                // to its end.
                let held = self
                    .locks
                    .contains(&lock::block_byte_of(self.lock_key(ti), oi, &entry.first));
                if self.begun.is_some() && !held {
                    continue;
                }
                // A block this writer staged, of a table with no unique key, holding rows its
                // updates replaced, is left for a later prune: a writer waiting on those rows
                // follows each to the row that replaced it.
                let staged = self.rebased.get(&(ti, oi)).is_some_and(|log| {
                    log.iter()
                        .any(|r| r.entries.iter().any(|e| e.child == entry.child))
                });
                if staged
                    && unique_key(&table).is_none()
                    && !read_map(&self.reader, &entry, &head.level_text)?
                        .chains
                        .is_empty()
                {
                    continue;
                }
                if !self.lock_blocks(ti, oi, vec![entry.clone()], Waiting::Try)? {
                    continue;
                }
                let (new, _) = self.write_block_afresh(
                    w,
                    &table,
                    order,
                    &head,
                    &entry,
                    Vec::new(),
                    Writing::Rewritten,
                )?;
                self.rebase((ti, oi), &entry, &new)?;
                replaced.insert(path, new);
                pruned += 1;
            }
            if replaced.is_empty() {
                continue;
            }
            settle(
                &mut top,
                &mut Vec::new(),
                &replaced,
                w,
                &mut Vec::new(),
                &head,
            )?;
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            self.root.orders[at].2 = w.place(&bytes)?;
        }
        Ok(pruned)
    }

    /// Every column of a block, whole: each of its leaves decoded, and the column made of them, in
    /// the order's columns' order.
    pub(super) fn block_columns(
        &self,
        table: &Table,
        order: &Order,
        map: &PageMap,
    ) -> Result<Vec<Values>> {
        let positions = map.rows() as usize;
        let leaves = order.leaves(table);
        let descs = leaf_descriptors(table, order)?;
        let mut columns = Vec::with_capacity(order.columns.len());
        for &c in &order.columns {
            let mut parts = Vec::new();
            for s in order.slots(table, c).unwrap() {
                let cp = &map.columns[s];
                let dictionary = cp
                    .dictionary
                    .map(|d| self.reader.read_extent(d))
                    .transpose()?;
                let pages = cp
                    .pages
                    .iter()
                    .map(|p| self.reader.read_extent(p.at))
                    .collect::<Result<Vec<_>>>()?;
                let slices: Vec<&[u8]> = pages.iter().map(Vec::as_slice).collect();
                parts.push(decode_leaf(
                    &descs[s],
                    &leaves[s].1,
                    dictionary.as_deref(),
                    &slices,
                    positions,
                )?);
            }
            columns.push(Values::from_leaves(table.columns[c].ty, parts));
        }
        Ok(columns)
    }

    /// One block's effects written: the dead marked and the new rows written after each column's
    /// last rows; or the block rewritten in order, where `writing` asks it, where it would hold more
    /// rows than a row group, or where rows come and the cut of its rows would make it more than one
    /// block. On the version a transaction of one snapshot reads, its rows stay after the block's
    /// last, whatever its cut, and the newest version alone says whether a unique key is shared.
    /// Returns the entries that stand for it now, and whether it was rewritten.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn write_block_afresh(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        head: &Head,
        entry: &Entry,
        effects: Vec<Effect>,
        writing: Writing,
    ) -> Result<(Vec<Entry>, bool)> {
        let mut map = read_map(&self.reader, entry, &head.level_text)?;
        let positions = map.rows() as usize;
        let leaves = order.leaves(table);
        let descs = leaf_descriptors(table, order)?;
        let columns = self.block_columns(table, order, &map)?;
        let mut dead: std::collections::BTreeSet<u32> = map.dead.iter().copied().collect();
        let types: Vec<ColumnType> = order.columns.iter().map(|&c| table.columns[c].ty).collect();
        let holds = |p: usize, values: &[Datum]| -> bool {
            columns.iter().zip(values).zip(&types).all(|((v, d), &ty)| {
                super::image::held_alike(&[datum_typed(v, p, ty)], std::slice::from_ref(d))
            })
        };
        let level_slots: Vec<usize> = order
            .levels
            .iter()
            .map(|l| order.slot(l.column()).unwrap())
            .collect();
        let mut gone: Vec<(usize, Vec<Datum>)> = Vec::new();
        let mut added: Vec<(Vec<KeyValue>, Vec<Datum>)> = Vec::new();
        for e in effects {
            if e.delete {
                let p = (0..positions)
                    .find(|&p| !dead.contains(&(p as u32)) && holds(p, &e.values))
                    .ok_or_else(|| {
                        // A commit's rows, found on its snapshot, met one changed since by a writer
                        // it did not wait for.
                        if self.committing {
                            return super::beside::concurrent_update();
                        }
                        Error::new(format!(
                            "{}: a row the statement found is not in {}",
                            table.name, order.name
                        ))
                    })?;
                dead.insert(p as u32);
                if let Some(to) = e.chain {
                    map.chains.push((p as u32, to));
                }
                gone.push((p, e.values));
            } else {
                added.push((e.key, e.values));
            }
        }
        // The block's live rows, and the new ones. Where rows come, they are laid out as a write lays
        // them out, which refuses two that share a unique order's key.
        let rows_now = || {
            let mut rows = Rows {
                columns: table.columns.iter().map(|c| Values::new(c.ty)).collect(),
            };
            let live: Vec<usize> = (0..positions)
                .filter(|p| !dead.contains(&(*p as u32)))
                .collect();
            let n = live.len() + added.len();
            for c in 0..table.columns.len() {
                match order.slot(c) {
                    Some(s) => {
                        for &p in &live {
                            push(&mut rows.columns[c], &datum_typed(&columns[s], p, types[s]));
                        }
                        for (_, row) in &added {
                            push(&mut rows.columns[c], &row[s]);
                        }
                    }
                    None => {
                        for _ in 0..n {
                            push(&mut rows.columns[c], &Datum::Null);
                        }
                    }
                }
            }
            rows
        };
        let mut rows = None;
        let split = !added.is_empty() && writing != Writing::Beside && {
            let r = rows_now();
            let cut = laid_out(table, order, &r, Some(&head.level_text))?
                .blocks
                .len()
                > 1;
            rows = Some(r);
            cut
        };
        if writing == Writing::Rewritten
            || split
            || (writing != Writing::Beside && positions + added.len() > ROW_GROUP_ROWS)
        {
            // Written in order, and cut by the counts as a block written afresh is.
            let rows = rows.unwrap_or_else(rows_now);
            let mut written = self.blocks_written(w, table, order, head, &rows)?;
            crate::directory::separate(
                &mut written,
                &head.level_text,
                Some(&entry.first),
                Some(entry.bound()),
            );
            return Ok((written, true));
        }
        // The new rows go after the block's last, into its tail: plain pages of one disk page, each
        // written afresh as rows come, the k-page pages before them left as they lie.
        let orderers = order
            .levels
            .iter()
            .map(|l| table.columns[l.column()].orderer())
            .collect::<Result<Vec<_>>>()?;
        let key_at = |p: usize| -> Vec<KeyValue> {
            order
                .levels
                .iter()
                .zip(&level_slots)
                .enumerate()
                .map(|(li, (&l, &s))| {
                    let column = &table.columns[l.column()];
                    let d = datum_typed(&columns[s], p, column.ty);
                    level_key(column, orderers[li], l, &d, null_key(order, li))
                })
                .collect()
        };
        // Each row added and each row gone, its values as the order's leaves hold them.
        let in_leaves = |values: &[Datum]| -> Vec<Datum> {
            order
                .columns
                .iter()
                .zip(values)
                .flat_map(|(&c, d)| leaf_datums(table, c, d))
                .collect()
        };
        let added_leaves: Vec<Vec<Datum>> = added.iter().map(|(_, r)| in_leaves(r)).collect();
        let gone_leaves: Vec<Vec<Datum>> = gone.iter().map(|(_, r)| in_leaves(r)).collect();
        if !added.is_empty() {
            for (s, (_, leaf)) in leaves.iter().enumerate() {
                let ty = leaf.ty;
                let cp = &map.columns[s];
                let last = cp
                    .pages
                    .last()
                    .expect("a block's column holds a page")
                    .clone();
                // The tail's last page is written afresh with the new rows; a page of rows in the
                // order's order stays, and the tail begins after it.
                let in_tail = last.first_row >= map.sorted;
                let mut values = Values::new(ty);
                let held = if in_tail {
                    let dictionary = cp
                        .dictionary
                        .map(|d| self.reader.read_extent(d))
                        .transpose()?;
                    let bytes = self.reader.read_extent(last.at)?;
                    Some(decode_leaf(
                        &descs[s],
                        leaf,
                        dictionary.as_deref(),
                        &[bytes.as_slice()],
                        last.rows as usize,
                    )?)
                } else {
                    None
                };
                if let Some((r, index)) = leaf.nested {
                    // A leaf of a composed column, or of a path's, a polygon's or a snapshot's
                    // tree: the rows held, then the new rows striped onto it.
                    let mut new = match r.def() {
                        crate::composed::Composed::Held(held) => {
                            let values: Vec<Datum> =
                                added_leaves.iter().map(|row| row[s].clone()).collect();
                            crate::composed::stripe_held(*held, &values)
                        }
                        _ => {
                            let whole: Vec<Option<std::sync::Arc<crate::composed::Compound>>> =
                                added_leaves
                                    .iter()
                                    .map(|row| match &row[s] {
                                        Datum::Compound(c) => Some(c.clone()),
                                        _ => None,
                                    })
                                    .collect();
                            crate::composed::stripe(r, &whole)
                        }
                    };
                    let new = new.swap_remove(index as usize);
                    let mut leaf_values = match held {
                        Some(Values::Nested(n)) => *n,
                        _ => crate::composed::Nested::empty(&new),
                    };
                    for row in 0..new.rows() {
                        leaf_values.push_from(&new, row);
                    }
                    values = Values::Nested(Box::new(leaf_values));
                } else {
                    if let Some(held) = held {
                        for i in 0..held.len() {
                            push(&mut values, &datum_typed(&held, i, ty));
                        }
                    }
                    for row in &added_leaves {
                        push(&mut values, &row[s]);
                    }
                }
                let base = if in_tail {
                    last.first_row
                } else {
                    last.first_row + last.rows
                } as usize;
                let mut fresh = Vec::new();
                // A value wider than the block's unit takes the page it needs, within the caps.
                for p in tail_pages(&descs[s], ty, &values, crate::cut::DATA_PAGE_PAGES)?.pages {
                    let row = base + p.first_row;
                    fresh.push(PageEntry {
                        at: w.place(&p.bytes)?,
                        first_row: row as u32,
                        rows: p.rows as u32,
                        first: if row < positions {
                            key_at(row)
                        } else {
                            added[row - positions].0.clone()
                        },
                    });
                }
                let pages = &mut map.columns[s].pages;
                if in_tail {
                    pages.pop();
                }
                pages.extend(fresh);
            }
        }
        // The page map and the entry as they stand now.
        let mut entry = entry.clone();
        map.dead = dead.iter().copied().collect();
        // Where no row of the block held a value of a column, live or dead, its least and greatest
        // mean nothing yet.
        let valueless: Vec<bool> = entry
            .columns
            .iter()
            .map(|sum| sum.nulls == entry.rows && entry.dead == 0)
            .collect();
        entry.rows = entry.rows + added.len() as u32 - gone.len() as u32;
        entry.dead += gone.len() as u32;
        for (key, _) in &added {
            crate::directory::widened(&mut entry, key);
        }
        let mut tails = std::mem::take(&mut entry.tails);
        for s in 0..leaves.len() {
            let sum = &mut entry.columns[s];
            let mut first_value = valueless[s];
            for row in &added_leaves {
                if row[s] == Datum::Null {
                    sum.nulls += 1;
                } else if let Some(v) = row[s]
                    .wide()
                    .filter(|_| !head.bound_slots.contains(&(s as u32)))
                {
                    if first_value {
                        sum.min = v;
                        sum.max = v;
                        first_value = false;
                    } else {
                        sum.min = sum.min.min(v);
                        sum.max = sum.max.max(v);
                    }
                }
            }
            for values in &gone_leaves {
                if values[s] == Datum::Null {
                    sum.nulls -= 1;
                }
            }
            // A column keyed by its bytes widens its bounds, its first 8 bytes kept in its first leaf's,
            // by each row added.
            if let Some(k) = head.bound_slots.iter().position(|&x| x as usize == s) {
                let ci = order
                    .columns
                    .iter()
                    .position(|&c| order.leaf_slot(table, c) == Some(s))
                    .unwrap();
                let column = &table.columns[order.columns[ci]];
                let orderer = column.orderer()?;
                let mut first_value = valueless[s];
                for (_, row) in &added {
                    let key = match &row[ci] {
                        Datum::Compound(c) => Some(c.key.to_vec()),
                        Datum::Text(t) => Some(
                            crate::strings::column_key(column.ty, orderer, t.as_bytes())
                                .into_owned(),
                        ),
                        Datum::Bytes(b) => {
                            Some(crate::strings::column_key(column.ty, orderer, b).into_owned())
                        }
                        _ => None,
                    };
                    if let Some(v) = key {
                        if tails.len() <= k {
                            tails.resize(head.bound_slots.len(), Default::default());
                        }
                        if first_value {
                            tails[k] = crate::directory::bounds_of(sum, &v, &v);
                            first_value = false;
                        } else {
                            crate::directory::bounds_widened(sum, &mut tails[k], &v);
                        }
                    }
                }
            }
            let cp = &map.columns[s];
            sum.pages = (cp.dictionary.map_or(0, Extent::pages)
                + cp.pages.iter().map(|p| p.at.pages()).sum::<u64>())
                as u32;
            if let Some(mi) = head.month_slots.iter().position(|&x| x as usize == s) {
                // A month's count, or an infinite instant's at its end.
                let count = |d: &Datum, entry: &mut Entry, by: i64| {
                    if let Datum::Int(v) = d {
                        let n = match month_of(*v) {
                            Some(m) => &mut entry.months[mi][m as usize - 1],
                            None => &mut entry.ends[mi][(*v > 0) as usize],
                        };
                        *n = (*n as i64 + by) as u32;
                    }
                };
                for row in &added_leaves {
                    count(&row[s], &mut entry, 1);
                }
                for values in &gone_leaves {
                    count(&values[s], &mut entry, -1);
                }
            }
            if let Some(t) = head.text_slots.iter().position(|&x| x as usize == s) {
                let bytes = |d: &Datum| match d {
                    Datum::Text(x) => x.len() as u32,
                    Datum::Bytes(x) => x.len() as u32,
                    _ => 0,
                };
                for row in &added_leaves {
                    entry.bytes[t] += bytes(&row[s]);
                }
                for values in &gone_leaves {
                    entry.bytes[t] -= bytes(&values[s]);
                }
            }
        }
        // A spanned column's span widens to hold each row added; a row gone leaves it as it was.
        for (k, c) in crate::spans::columns(table, order).into_iter().enumerate() {
            let at = order.slot(c).unwrap();
            for (_, values) in &added {
                crate::spans::widen(&mut entry.spans[k], table.columns[c].ty, &values[at]);
            }
        }
        // A BRIN summary sums each row added; a row gone leaves it as it was.
        for (k, (level, kind)) in crate::brin::kept(table, order).into_iter().enumerate() {
            let at = order.slot(level.column()).unwrap();
            for (_, values) in &added {
                crate::brin::add_row(
                    &mut entry.summed[k],
                    table,
                    level,
                    kind,
                    &values[at],
                    entry.rows,
                )?;
            }
        }
        entry.tails = tails;
        entry.child = w.place(&map.encode())?;
        Ok((vec![entry], false))
    }

    /// Rows of an order written as new blocks, cut by the counts at each sublevel of the order's key.
    fn blocks_written(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        head: &Head,
        rows: &Rows,
    ) -> Result<Vec<Entry>> {
        let Laid {
            perm,
            keys,
            blocks,
            widths,
            ..
        } = laid_out(table, order, rows, Some(&head.level_text))?;
        let descs = leaf_descriptors(table, order)?;
        // Each block takes the unit of its own furthest-out place.
        let block_units: Vec<Vec<usize>> =
            crate::cut::largest_places(&keys, &blocks, order, table, &widths)
                .iter()
                .map(|m| {
                    (0..widths.0.len())
                        .map(|s| widths.unit(s, m[s], rows.len()))
                        .collect()
                })
                .collect();
        let month_slots: Vec<usize> = head.month_slots.iter().map(|&s| s as usize).collect();
        let text_slots: Vec<usize> = head.text_slots.iter().map(|&s| s as usize).collect();
        let mut entries = Vec::with_capacity(blocks.len());
        for (b, units) in blocks.iter().zip(&block_units) {
            let (entry, _, _) = write_block(
                w,
                table,
                order,
                rows,
                &perm,
                &keys,
                b,
                &descs,
                units,
                &month_slots,
                &text_slots,
                &[],
                &mut MapPages::new(false),
            )?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The rows an empty order is given, as its first blocks, and the head they give the order: its
    /// units and its text levels' sublevels, as a snail written from them would hold.
    pub(super) fn written_in_order(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        held: Vec<Vec<Datum>>,
    ) -> Result<(Vec<Entry>, Head)> {
        let mut rows = Rows {
            columns: table.columns.iter().map(|c| Values::new(c.ty)).collect(),
        };
        for (c, values) in rows.columns.iter_mut().enumerate() {
            for r in &held {
                push(values, order.slot(c).map_or(&Datum::Null, |s| &r[s]));
            }
        }
        let laid = laid_out(table, order, &rows, None)?;
        let head = head_of(table, order, &laid, held.len());
        let mut written = self.blocks_written(w, table, order, &head, &rows)?;
        crate::directory::separate(&mut written, &head.level_text, None, None);
        Ok((written, head))
    }

    /// A table, an index or a table's order declared: the catalog read again with it, and each order
    /// it adds or changes empty, until rows come. A table that holds rows keeps its columns and
    /// orders as written.
    /// Refuses a domain's NOT NULL set, or a CHECK of its added valid or validated, that a value one
    /// of its columns holds, or a column of a domain over it, does not meet, as PostgreSQL's ALTER
    /// DOMAIN validates them: each table in turn, each row, each of its columns, in PostgreSQL's
    /// words. `catalog` is the catalog the DDL declares; the rows are the file's.
    fn domains_validated(&mut self, catalog: &Catalog) -> Result<()> {
        fn derives(domains: &[crate::catalog::Domain], name: &str, of: &str) -> bool {
            name == of
                || domains
                    .iter()
                    .find(|d| d.name == name)
                    .is_some_and(|d| derives(domains, &d.base, of))
        }
        for d in &catalog.domains {
            let was = self.catalog.domains.iter().find(|x| x.name == d.name);
            let not_null = d.not_null && !was.is_some_and(|w| w.not_null);
            let checks: Vec<&crate::catalog::DomainCheck> = d
                .checks
                .iter()
                .filter(|k| k.valid)
                .filter(|k| {
                    !was.is_some_and(|w| w.checks.iter().any(|x| x.name == k.name && x.valid))
                })
                .collect();
            if !not_null && checks.is_empty() {
                continue;
            }
            let held: Vec<(Table, Vec<usize>)> = self
                .catalog
                .tables
                .iter()
                .filter_map(|t| {
                    let cols: Vec<usize> = (0..t.columns.len())
                        .filter(|&c| {
                            t.columns[c]
                                .domain
                                .as_deref()
                                .is_some_and(|n| derives(&catalog.domains, n, &d.name))
                        })
                        .collect();
                    (!cols.is_empty()).then(|| (t.clone(), cols))
                })
                .collect();
            for (t, cols) in held {
                let names = cols
                    .iter()
                    .map(|&c| format!("\"{}\"", t.columns[c].name.replace('"', "\"\"")))
                    .collect::<Vec<_>>()
                    .join(", ");
                let (b, _) = self.answer(&format!("SELECT {names} FROM {}", t.name), None)?;
                let bare = t.name.rsplit('.').next().unwrap_or(&t.name);
                for r in rows_of(&b)? {
                    for (k, &c) in cols.iter().enumerate() {
                        let col = &t.columns[c];
                        if not_null && r[k] == Datum::Null {
                            return Err(Error::new(format!(
                                "column \"{}\" of table \"{bare}\" contains null values",
                                col.name
                            )));
                        }
                        for check in &checks {
                            if !fill::check_holds(&check.check, col.ty, &r[k], &t)? {
                                return Err(Error::new(format!(
                                    "column \"{}\" of table \"{bare}\" contains values that \
                                     violate the new constraint",
                                    col.name
                                )));
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// A declaration: the catalog read again with the statement after its DDL, each order it makes
    /// or changes laid out, each a DROP takes out let go. Returns what PostgreSQL says of it.
    fn declare(&mut self, ddl: &str) -> Result<Vec<String>> {
        self.declare_after(ddl, &[])
    }

    /// A declaration, as `declare`, begun again after waiting for another writer's lock on each
    /// type or function of `waited` its new objects depend on: refused, as PostgreSQL refuses it
    /// once it holds the lock, where that writer's end took one out.
    fn declare_after(&mut self, ddl: &str, waited: &[crate::oids::Object]) -> Result<Vec<String>> {
        // Its names are looked up, and its relations made, on the catalog as committed now: where
        // another writer's commit moved it past the catalog the transaction declares on, the
        // transaction's own statements are declared again on it first.
        self.snapshot_kept();
        let newest = self.reader.peek_root()?;
        let from = self
            .declared
            .as_ref()
            .map_or(self.root.catalog_named(), |d| d.from);
        let own = self.declared.as_ref().map_or("", |d| d.own.as_str());
        let given = self.declared.as_ref().map_or(&[][..], |d| d.given.as_slice());
        if let Some(committed) = self.committed_ddl(&newest, from, own, given)? {
            self.lay_over(committed)?;
        }
        let stands = |o: &crate::oids::Object| self.catalog.oids.of(o).is_some();
        if !waited.iter().all(stands) {
            let what = match waited.iter().find(|o| !stands(o)) {
                Some(crate::oids::Object::Proc(..)) => "function",
                _ => "type",
            };
            return Err(Error::new(format!("referenced {what} was concurrently dropped")));
        }
        // The relations it changes are locked in PostgreSQL's modes; where it waited for another
        // writer's, it begins again on the catalog that writer's end leaves.
        if self.ddl_locked(ddl)? {
            return self.declare_after(ddl, waited);
        }
        let from = newest.catalog_named();
        // Joined to the kept text's last statement ended, as psql ends a file's last statement.
        let base = format!("{}\n", crate::ddl::ended(&self.catalog.ddl)?.trim_end());
        let text = format!("{base}{ddl};\n");
        let kept = Catalog::statements_in(&self.catalog.ddl)?;
        let its_place = kept;
        let (release, monetary) = (self.root.release, self.root.monetary.clone());
        let searched = self.root.text_search().to_string();
        let prepared = |catalog: &mut Catalog| -> Result<()> {
            catalog.with_release(release)?;
            catalog.with_monetary(&crate::file::monetary_of(catalog, &monetary))?;
            catalog.with_text_search(&searched)
        };
        // Each literal PostgreSQL keeps as its value where the statement is declared, kept so,
        // under the statement's zone and on its clock.
        let text = crate::deparse::fold::folded(&text, kept, kept, &prepared)?.into_owned();
        let stated = text
            .strip_prefix(base.as_str())
            .ok_or_else(|| Error::new("the catalog's text moved as its last statement was read"))?
            .to_string();
        // The statements before it keep the OIDs they were given; it goes on from them for now.
        let record = self.catalog.oids.departures();
        let (mut catalog, said) = match Catalog::parse_declared(&text, kept, &record) {
            Ok(read) => read,
            Err(e) => return Err(self.reached_holding_rows(e, &newest)?),
        };
        prepared(&mut catalog)?;
        // The relations it reads are locked as PostgreSQL's parse analysis locks them; where it
        // waited for another writer's, it begins again on the catalog that writer's end leaves.
        if self.reads_locked(&catalog)? {
            return self.declare_after(ddl, waited);
        }
        // The types and functions it takes out, and those its new objects depend on, likewise.
        if let Some(read) = self.depended_locked(&catalog)? {
            let mut waited = waited.to_vec();
            waited.extend(read);
            return self.declare_after(ddl, &waited);
        }
        // A name another writer is making and has not committed is waited for; where its end
        // leaves the name standing, refused there, and else the declaration begins again on the
        // catalog its end leaves.
        if self.names_taken(&catalog, from)? {
            return self.declare_after(ddl, waited);
        }
        let mut made: Vec<(String, String)> = Vec::new();
        let mut kept: Vec<(String, String)> = Vec::new();
        for t in &catalog.tables {
            let old = self.catalog.tables.iter().find(|x| x.name == t.name);
            // A strand made new is a change to the table it is of.
            if let (None, Some(of)) = (old, &t.elements_of) {
                let source = self.catalog.table(of);
                if source.map(|o| self.holds_rows_now(o, &newest)).transpose()? == Some(true) {
                    return Err(Error::new(format!(
                        "{of}: it holds rows, so its columns and orders stay as written"
                    )));
                }
            }
            let columns_same = old.is_some_and(|o| {
                o.columns.len() == t.columns.len()
                    && o.columns
                        .iter()
                        .zip(&t.columns)
                        .all(|(a, b)| a.name == b.name && a.ty == b.ty && a.not_null == b.not_null)
            });
            let mut changed = false;
            for o in &t.orders {
                let was = old.and_then(|x| x.orders.iter().find(|y| y.name == o.name));
                let same = columns_same
                    && was.is_some_and(|w| {
                        w.levels == o.levels
                            && w.columns == o.columns
                            && w.unique == o.unique
                            && w.table_order == o.table_order
                            && (!o.table_order || old.is_some_and(|x| x.brins == t.brins))
                    });
                if same {
                    kept.push((t.name.clone(), o.name.clone()));
                } else {
                    made.push((t.name.clone(), o.name.clone()));
                    changed = true;
                }
            }
            if let (true, Some(o)) = (changed, old) {
                if self.holds_rows_now(o, &newest)? {
                    return Err(Error::new(format!(
                        "{}: it holds rows, so its columns and orders stay as written",
                        t.name
                    )));
                }
            }
            // A CHECK added valid, or validated, is checked against the rows the table holds.
            if let Some(o) = old {
                for k in t.checks.iter().filter(|k| k.valid && k.enforced) {
                    if o.checks.iter().any(|x| x.name == k.name && x.valid) {
                        continue;
                    }
                    let columns = t
                        .columns
                        .iter()
                        .map(|c| format!("\"{}\"", c.name.replace('"', "\"\"")))
                        .collect::<Vec<_>>()
                        .join(", ");
                    let q = format!(
                        "WITH r AS (SELECT {columns} FROM {}) SELECT count(*) AS n FROM r WHERE ({}) IS FALSE",
                        t.name, k.expr
                    );
                    let (b, _) = self.answer(&q, None)?;
                    let n = b
                        .column(0)
                        .as_any()
                        .downcast_ref::<Int64Array>()
                        .map_or(0, |a| a.value(0));
                    if n > 0 {
                        return Err(Error::new(format!(
                            "check constraint \"{}\" of relation \"{}\" is violated by some row",
                            k.name,
                            t.name.rsplit('.').next().unwrap_or(&t.name)
                        )));
                    }
                }
            }
        }
        self.domains_validated(&catalog)?;
        // A foreign key added valid, or validated, on a table that holds rows is checked against
        // every row, as the catalog declared reads it.
        let mut validated: Vec<(usize, usize)> = Vec::new();
        for (ci, t) in catalog.tables.iter().enumerate() {
            let Some(o) = self.catalog.tables.iter().find(|x| x.name == t.name) else {
                continue;
            };
            for (fi, k) in t.foreign_keys.iter().enumerate() {
                let was = o.foreign_keys.iter().any(|x| x.name == k.name && x.valid);
                if k.valid && k.enforced && !was && self.holds_rows(o)? {
                    validated.push((ci, fi));
                }
            }
        }
        if !validated.is_empty() {
            let before = std::mem::replace(&mut self.catalog, catalog.clone());
            let mut checked = Ok(true);
            for &(ci, fi) in &validated {
                checked = self.key_validated(ci, fi);
                if !matches!(checked, Ok(true)) {
                    break;
                }
            }
            self.catalog = before;
            if !checked? {
                return Ok(Vec::new());
            }
        }
        // Its objects take their OIDs from the counter every writer of the file shares: in a
        // transaction block at once, at the root's turn, as PostgreSQL's are never given back;
        // else at its commit. Where it takes others than the text went on to, it is read again
        // with them.
        let mut took = None;
        if self.begun.is_some() {
            if let Some(run) = catalog.oids.run_at(its_place) {
                let place = match self.declared.as_ref() {
                    Some(d) => Catalog::statements_in(&d.own)?,
                    None => 0,
                };
                let on = catalog.oids.going_on_at(its_place - place);
                let first = self.at_oids_turn(run.n, from, on)?;
                if first != run.first {
                    let mut record = record;
                    record.push((its_place as u32, first));
                    (catalog, _) = Catalog::parse_declared(&text, its_place, &record)?;
                    prepared(&mut catalog)?;
                }
                took = Some(Took {
                    place,
                    first,
                    n: run.n,
                });
            }
        }
        let mut stage = self.reader.stage();
        let mut orders: Vec<(String, String, Extent)> = self
            .root
            .orders
            .iter()
            .filter(|(t, o, _)| kept.contains(&(t.clone(), o.clone())))
            .cloned()
            .collect();
        for (tn, on) in &made {
            let t = catalog.table(tn).expect("a table the catalog names");
            let o = t.orders.iter().find(|x| x.name == *on).unwrap();
            let top = stage.place(&encode_top(&empty_head(t, o), &[]))?;
            orders.push((tn.clone(), on.clone(), top));
        }
        self.root.catalog = stage.place(text.as_bytes())?;
        if let Some(cut) = self.root.cut.as_mut() {
            cut.catalog_at = None;
        }
        self.root.orders = orders;
        // A sequence made or restarted stands where its DDL leaves it, whatever the file kept.
        let resets: Vec<String> = catalog
            .sequences
            .iter()
            .filter(|s| {
                self.catalog
                    .sequence(&s.name)
                    .is_none_or(|o| (o.last_value, o.is_called) != (s.last_value, s.is_called))
            })
            .map(|s| s.name.clone())
            // A sequence dropped keeps no state either.
            .chain(
                self.catalog
                    .sequences
                    .iter()
                    .filter(|s| catalog.sequence(&s.name).is_none())
                    .map(|s| s.name.clone()),
            )
            .collect();
        // The orders a DROP took out: of a table it dropped, or an index it took with it.
        let mut taken_out: Vec<(String, String)> = Vec::new();
        for t in &self.catalog.tables {
            for o in &t.orders {
                let stands = catalog
                    .table(&t.name)
                    .is_some_and(|x| x.orders.iter().any(|y| y.name == o.name));
                if !stands {
                    taken_out.push((t.name.clone(), o.name.clone()));
                }
            }
        }
        // The relations it made, and those it dropped.
        let relations = |c: &Catalog| -> Vec<String> {
            c.tables
                .iter()
                .filter(|t| t.declared())
                .map(|t| t.name.clone())
                .chain(c.views.iter().map(|v| v.name.clone()))
                .chain(c.sequences.iter().map(|s| s.name.clone()))
                .collect()
        };
        let (before, after) = (relations(&self.catalog), relations(&catalog));
        // The schemas it made, and those it dropped.
        let schemas = |c: &Catalog| -> Vec<String> {
            c.oids
                .each()
                .filter_map(|(_, o)| match o {
                    crate::oids::Object::Namespace(n) => Some(n.clone()),
                    _ => None,
                })
                .collect()
        };
        let (had, has) = (schemas(&self.catalog), schemas(&catalog));
        // The types, domains and functions it made, and those it dropped.
        let objects = |c: &Catalog| -> Vec<crate::oids::Object> {
            use crate::oids::{full, Object as O};
            c.types
                .iter()
                .map(|t| O::Type(full(&t.name)))
                .chain(c.domains.iter().map(|d| O::Type(full(&d.name))))
                .chain(c.functions.iter().map(crate::pgcatalog::function_object))
                .collect()
        };
        let (had_objects, objects_now) = (objects(&self.catalog), objects(&catalog));
        self.renumbered(&catalog);
        self.sequences_redeclared(&catalog);
        self.catalog = catalog;
        let d = self.declared.get_or_insert(Declared {
            from,
            ddl: String::new(),
            own: String::new(),
            made: Vec::new(),
            resets: Vec::new(),
            taken_out: Vec::new(),
            relations: Vec::new(),
            schemas: Vec::new(),
            objects: Vec::new(),
            given: Vec::new(),
        });
        d.given.extend(took);
        for r in resets {
            if !d.resets.contains(&r) {
                d.resets.push(r);
            }
        }
        d.relations.retain(|r| after.contains(r));
        d.relations
            .extend(after.into_iter().filter(|r| !before.contains(r)));
        d.schemas.retain(|s| has.contains(s));
        d.schemas.extend(has.into_iter().filter(|s| !had.contains(s)));
        d.objects.retain(|o| objects_now.contains(o));
        d.objects.extend(objects_now.into_iter().filter(|o| !had_objects.contains(o)));
        d.from = from;
        d.ddl = text;
        d.own.push_str(&stated);
        // An order made and then taken out is made no more; one taken out and made again is made.
        d.made.retain(|m| !taken_out.contains(m));
        for t in taken_out {
            if !d.taken_out.contains(&t) {
                d.taken_out.push(t);
            }
        }
        for m in made {
            if !d.made.contains(&m) {
                d.made.push(m);
            }
        }
        Ok(said)
    }

    /// Takes, alone and to the transaction's end, the byte of each key of the catalog's unique
    /// indexes that an object `catalog` makes takes, in the order PostgreSQL writes their rows,
    /// waiting where another writer making the same key holds it. Returns whether it waited:
    /// refused, in PostgreSQL's words for the index, where that writer's commit leaves the key
    /// standing on the catalog as committed, which moved past `from`.
    fn names_taken(&mut self, catalog: &Catalog, from: (Extent, u64)) -> Result<bool> {
        if self.writer.is_none() {
            return Ok(false);
        }
        for o in names_made(&self.catalog, catalog) {
            let Some(key) = crate::pgcatalog::name_key(catalog, &o) else {
                continue;
            };
            let b = lock::name_byte(key.index, &key.names);
            if self.locks.contains(&b) {
                continue;
            }
            if self.lock_byte(b, Waiting::Try)? {
                self.locks.insert(b);
                continue;
            }
            self.lock_byte_for(b, false, "a name another writer is making")?;
            self.locks.insert(b);
            let newest = self.reader.peek_root()?;
            if let Some(committed) = self.committed_ddl(&newest, from, "", &[])? {
                let standing = committed.oids.each().find_map(|(_, x)| {
                    crate::pgcatalog::name_key(&committed, x)
                        .filter(|k| k.index == key.index && k.names == key.names)
                });
                if let Some(k) = standing {
                    return Err(k.duplicate());
                }
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// A statement of a transaction of one snapshot names as committed: where another writer's
    /// commit moved the catalog past the one it reads, the transaction is laid over the catalog as
    /// committed, its own statements declared on it, reading its snapshot's rows (`lay_over`); and
    /// its catalog tables' rows are made of its snapshot's objects and its own.
    pub(super) fn named_as_committed(&mut self) -> Result<()> {
        if self.begun.is_none() || !self.held {
            return Ok(());
        }
        let newest = self.reader.peek_root()?;
        let from = self
            .declared
            .as_ref()
            .map_or(self.root.catalog_named(), |d| d.from);
        let own = self.declared.as_ref().map_or(String::new(), |d| d.own.clone());
        let given = self.declared.as_ref().map_or(Vec::new(), |d| d.given.clone());
        let Some(committed) = self.committed_ddl(&newest, from, &own, &given)? else {
            self.seen = self.catalog_seen(&newest)?;
            return Ok(());
        };
        self.snapshot_kept();
        self.lay_over(committed)?;
        match self.declared.as_mut() {
            Some(d) => {
                d.from = newest.catalog_named();
                d.ddl = self.catalog.ddl.clone();
            }
            // Its root names the catalog it now reads, as committed.
            None => {
                self.root.catalog = newest.catalog;
                if let (Some(c), Some(n)) = (self.root.cut.as_mut(), newest.cut.as_ref()) {
                    c.catalog_at = n.catalog_at;
                }
            }
        }
        self.seen = self.catalog_seen(&newest)?;
        Ok(())
    }

    /// A statement whose round renews, alone or in a transaction at READ COMMITTED, names as
    /// committed: where another writer's commit moved the catalog past the one the snail read last,
    /// or the one a transaction that declared declared on, its round renews before the statement
    /// is read.
    fn renewed_where_declared_since(&mut self) -> Result<()> {
        if self.held || (self.begun.is_none() && self.rounds == 0) {
            return Ok(());
        }
        let read = self
            .declared
            .as_ref()
            .map_or(self.root.catalog_named(), |d| d.from);
        if self.reader.peek_root()?.catalog_named() == read {
            return Ok(());
        }
        match self.begun.is_some() {
            true => self.renew_in_transaction()?,
            false => self.renew()?,
        };
        Ok(())
    }

    /// Keeps, once, the catalog of a transaction of one snapshot as its snapshot names it, before
    /// its own DDL or another's moves it.
    fn snapshot_kept(&mut self) {
        if self.held && self.snapshot_catalog.is_none() {
            self.snapshot_catalog = Some((self.catalog.clone(), self.root.catalog_named()));
        }
    }

    /// The catalog a transaction of one snapshot makes its catalog tables' rows from, where another
    /// writer's DDL moved the catalog since its snapshot: its snapshot's, where it declared nothing;
    /// else its own less what another made since, with what another dropped since. None where
    /// nothing moved it.
    fn catalog_seen(&self, newest: &Root) -> Result<Option<Catalog>> {
        let Some((snapshot, at)) = self.snapshot_catalog.as_ref() else {
            return Ok(None);
        };
        if newest.catalog_named() == *at {
            return Ok(None);
        }
        if self.declared.as_ref().is_none_or(|d| d.own.is_empty()) {
            return Ok(Some(snapshot.clone()));
        }
        let Some(committed) = self.committed_ddl(newest, *at, "", &[])? else {
            return Ok(None);
        };
        Ok(Some(crate::pgcatalog::seen(&self.catalog, &committed, snapshot)))
    }

    /// The transaction on `to`, the catalog as committed with its own statements declared on it:
    /// its root given an order for each `to` declares, its own staged and its snapshot's where the
    /// table stands as it read it, and empty, staged, where another writer made the table since or
    /// changed its columns or orders, so that it reads none of their rows; a table another dropped
    /// since given none. Refused where another's DDL since dropped or changed a table it wrote rows
    /// of, as its commit is.
    fn lay_over(&mut self, to: Catalog) -> Result<()> {
        self.wrote_stand(&to)?;
        let mut stage = self.reader.stage();
        let mut orders: Vec<(String, String, Extent)> = Vec::new();
        for t in &to.tables {
            let same = self.catalog.table(&t.name).is_some_and(|x| x.same_shape(t));
            for o in &t.orders {
                let held = self
                    .root
                    .orders
                    .iter()
                    .find(|(a, b, _)| *a == t.name && *b == o.name)
                    .filter(|_| same);
                match held {
                    Some(h) => orders.push(h.clone()),
                    None => {
                        let head = empty_head(t, o);
                        let top = stage.place(&encode_top(&head, &[]))?;
                        orders.push((t.name.clone(), o.name.clone(), top));
                    }
                }
            }
        }
        self.root.orders = orders;
        self.renumbered(&to);
        self.sequences_redeclared(&to);
        self.catalog = to;
        Ok(())
    }

    /// Refuses a catalog in which a table the transaction wrote rows of no longer stands as it
    /// wrote them: dropped since, or its columns or orders changed.
    fn wrote_stand(&self, to: &Catalog) -> Result<()> {
        let rebased: Vec<&str> = self
            .rebased
            .keys()
            .map(|&(ti, _)| self.catalog.tables[ti].name.as_str())
            .collect();
        for t in &self.catalog.tables {
            let wrote = self.own.get(&t.name).is_some_and(|o| o.changed())
                || rebased.contains(&t.name.as_str());
            if wrote && !to.table(&t.name).is_some_and(|x| x.same_shape(t)) {
                return Err(super::beside::concurrent_update());
            }
        }
        Ok(())
    }

    /// The transaction's own statements declared again on the catalog the newest root names, which
    /// another writer's commit moved past the one it declared on, as its commit writes them there:
    /// refused in the words of the first refused, and where a table it wrote rows of no longer
    /// stands as it wrote them.
    fn replayed_on(&mut self, root: &Root) -> Result<()> {
        let Some(d) = self.declared.as_ref() else {
            return Ok(());
        };
        let Some(catalog) = self.committed_ddl(root, d.from, &d.own, &d.given)? else {
            return Ok(());
        };
        self.wrote_stand(&catalog)?;
        self.renumbered(&catalog);
        self.sequences_redeclared(&catalog);
        let d = self.declared.as_mut().expect("a transaction that declared");
        d.from = root.catalog_named();
        d.ddl = catalog.ddl.clone();
        self.catalog = catalog;
        Ok(())
    }

    /// The OIDs a commit that declared leaves its root, at the root's turn: each of its own
    /// statements that took none from the counter every writer of the file shares takes its own
    /// now, its statements read again where one takes others than the text went on to; the record
    /// of the statements whose OIDs depart from the text's order written where it changed; and the
    /// counter kept only where it is past where the text goes on.
    fn oids_at_commit(&mut self, root: &mut Root, w: &mut Writer, gone: &mut Vec<Extent>) -> Result<()> {
        let Some(d) = self.declared.as_ref() else {
            return Ok(());
        };
        let mine = Catalog::statements_in(&d.own)?;
        let committed = Catalog::statements_in(&self.catalog.ddl)? - mine;
        let mut given = d.given.clone();
        let mut next = match root.next_oid {
            0 => self.catalog.oids.going_on_at(committed),
            next => next,
        };
        let mut moved = false;
        for place in 0..mine {
            if given.iter().any(|t| t.place == place) {
                continue;
            }
            let Some(run) = self.catalog.oids.run_at(committed + place) else {
                continue;
            };
            moved |= run.first != next;
            given.push(Took {
                place,
                first: next,
                n: run.n,
            });
            next += run.n;
        }
        if moved {
            given.sort_by_key(|t| t.place);
            let catalog = self.own_on(root, &d.own, &given)?;
            self.sequences_redeclared(&catalog);
            self.catalog = catalog;
        }
        if let Some(d) = self.declared.as_mut() {
            d.given = given;
        }
        let record = self.catalog.oids.departures();
        if record != self.reader.oid_record(root)? {
            self.reader.put_oid_record(root, &record, w, gone)?;
        }
        let next = next.max(root.next_oid);
        root.next_oid = if next > self.catalog.oids.next() { next } else { 0 };
        Ok(())
    }

    /// What a statement of a transaction that declared reads on `root`, as its commit writes it
    /// there: its own statements declared again where another writer's commit moved the catalog
    /// since (`replayed_on`), the root's orders but those of a table its DDL made or changed and
    /// those a DROP of it took out, and each order it made empty, its own rows to come.
    fn declared_on(&mut self, root: &mut Root) -> Result<()> {
        let moved = self
            .declared
            .as_ref()
            .is_some_and(|d| root.catalog_named() != d.from);
        self.replayed_on(root)?;
        let d = self.declared.as_ref().expect("a transaction that declared");
        let mut stage = self.reader.stage();
        let touched: Vec<&String> = d.made.iter().map(|(t, _)| t).collect();
        let mut orders = Vec::new();
        for (t, o, e) in std::mem::take(&mut root.orders) {
            let made = d.made.contains(&(t.clone(), o.clone()));
            let declared = self
                .catalog
                .table(&t)
                .is_some_and(|x| x.orders.iter().any(|y| y.name == o));
            let taken_out = d.taken_out.contains(&(t.clone(), o.clone()));
            if !((touched.contains(&&t) && (made || !declared)) || taken_out) {
                orders.push((t, o, e));
            }
        }
        for (tn, on) in &d.made {
            if let Some(t) = self.catalog.table(tn) {
                if let Some(o) = t.orders.iter().find(|x| x.name == *on) {
                    let head = empty_head(t, o);
                    orders.push((tn.clone(), on.clone(), stage.place(&encode_top(&head, &[]))?));
                }
            }
        }
        root.orders = orders;
        // Its root names the catalog it declared, as a declaration stages it.
        root.catalog = match moved {
            true => stage.place(d.ddl.as_bytes())?,
            false => self.root.catalog,
        };
        if let Some(c) = root.cut.as_mut() {
            c.catalog_at = None;
        }
        Ok(())
    }

    /// What the statement or the transaction under way holds of each table by its place, and of
    /// each order and foreign key by its place in its table, moved to their places in the catalog
    /// `to` declares; what it held of one the declaration took out let go, its rows with it.
    fn renumbered(&mut self, to: &Catalog) {
        let from = &self.catalog;
        let tables: Vec<Option<usize>> = from
            .tables
            .iter()
            .map(|t| to.tables.iter().position(|x| x.name == t.name))
            .collect();
        let orders: Vec<Vec<Option<usize>>> = from
            .tables
            .iter()
            .zip(&tables)
            .map(|(t, at)| {
                t.orders
                    .iter()
                    .map(|o| at.and_then(|i| to.tables[i].orders.iter().position(|y| y.name == o.name)))
                    .collect()
            })
            .collect();
        let keys: Vec<Vec<Option<usize>>> = from
            .tables
            .iter()
            .zip(&tables)
            .map(|(t, at)| {
                t.foreign_keys
                    .iter()
                    .map(|k| {
                        at.and_then(|i| to.tables[i].foreign_keys.iter().position(|y| y.name == k.name))
                    })
                    .collect()
            })
            .collect();
        let unmoved = tables.iter().enumerate().all(|(i, t)| *t == Some(i))
            && orders.iter().all(|os| os.iter().enumerate().all(|(i, o)| *o == Some(i)))
            && keys.iter().all(|ks| ks.iter().enumerate().all(|(i, k)| *k == Some(i)));
        if unmoved {
            return;
        }
        let order = |(t, o): (usize, usize)| -> Option<(usize, usize)> {
            Some((tables.get(t).copied().flatten()?, orders.get(t)?.get(o).copied().flatten()?))
        };
        let key = |(t, k): (usize, usize)| -> Option<(usize, usize)> {
            Some((tables.get(t).copied().flatten()?, keys.get(t)?.get(k).copied().flatten()?))
        };
        let gone: Vec<String> = from
            .tables
            .iter()
            .zip(&tables)
            .filter(|(_, at)| at.is_none())
            .map(|(t, _)| t.name.clone())
            .collect();
        self.rebased = std::mem::take(&mut self.rebased)
            .into_iter()
            .filter_map(|(at, v)| Some((order(at)?, v)))
            .collect();
        self.unnamed = std::mem::take(&mut self.unnamed)
            .into_iter()
            .filter_map(|t| tables.get(t).copied().flatten())
            .collect();
        self.modes = std::mem::take(&mut self.modes)
            .into_iter()
            .filter_map(|(at, v)| Some((key(at)?, v)))
            .collect();
        self.touched = std::mem::take(&mut self.touched).renumbered(&key, &order);
        self.pending = std::mem::take(&mut self.pending).renumbered(&key, &order);
        self.own.retain(|t, _| !gone.contains(t));
    }

    /// What the session holds of a sequence a declaration took out or made again: the values its
    /// cache held, given up, as PostgreSQL's session gives up a dropped sequence's.
    fn sequences_redeclared(&mut self, to: &Catalog) {
        let oid = |c: &Catalog, name: &str| {
            c.oids
                .of(&crate::oids::Object::Class(crate::oids::full(name)))
        };
        let from = &self.catalog;
        self.handed.retain(|name, _| {
            from.sequence(name).is_none() || oid(from, name) == oid(to, name)
        });
    }

    /// Whether any order of a table holds a live row, as the round reads the file.
    fn holds_rows(&self, table: &Table) -> Result<bool> {
        self.orders_hold_rows(&self.root, &table.name)
    }

    /// A declaration's refusal, as the catalog gave it; where a drop reaches a column of a table
    /// that holds rows, refused as any change to such a table's columns is.
    fn reached_holding_rows(&self, e: Error, newest: &Root) -> Result<Error> {
        let Some(name) = crate::ddl::reached_table(&e) else {
            return Ok(e);
        };
        let table = self
            .catalog
            .tables
            .iter()
            .find(|t| crate::oids::full(&t.name) == name);
        match table {
            Some(t) if self.holds_rows_now(t, newest)? => Ok(Error::new(format!(
                "{}: it holds rows, so its columns and orders stay as written",
                t.name
            ))),
            _ => Ok(e),
        }
    }

    /// Whether any order of a table holds a live row, as the round reads the file or as `newest`
    /// names it: rows committed since the transaction's snapshot among them.
    fn holds_rows_now(&self, table: &Table, newest: &Root) -> Result<bool> {
        Ok(self.holds_rows(table)? || self.orders_hold_rows(newest, &table.name)?)
    }

    /// Whether any order of the table of that name holds a live row on `root`.
    fn orders_hold_rows(&self, root: &Root, table: &str) -> Result<bool> {
        for (t, _, top) in &root.orders {
            if t == table {
                let (_, entries) = decode_top(&self.reader.read_extent(*top)?)?;
                if entries.iter().any(|e| e.rows > 0) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

pub(super) fn entry_at<'a>(node: &'a Node, path: &[usize]) -> &'a Entry {
    match path {
        [i] => &node.entries[*i],
        [i, rest @ ..] => entry_at(&node.kids[i], rest),
        [] => unreachable!("a path names a block"),
    }
}

/// The first key of the block after the one a path names, where one follows: the entry beside the
/// path's on the lowest page that has one.
fn following<'a>(node: &'a Node, path: &[usize]) -> Option<&'a [KeyValue]> {
    let (&i, rest) = path.split_first()?;
    let here = node.entries.get(i + 1).map(|e| e.first.as_slice());
    match rest {
        [] => here,
        _ => following(&node.kids[&i], rest).or(here),
    }
}

/// Writes the directory pages under `node` that changed, from the blocks up: each block replaced by
/// the entries that stand for it now, and each page read on the way down written afresh, split where
/// it no longer fits a page. Leaves `node`'s own entries as they stand now; the pages written over
/// are added to `gone`.
pub(super) fn settle(
    node: &mut Node,
    path: &mut Vec<usize>,
    replaced: &BTreeMap<Vec<usize>, Vec<Entry>>,
    w: &mut dyn Place,
    gone: &mut Vec<Extent>,
    head: &Head,
) -> Result<()> {
    let kids: Vec<usize> = node.kids.keys().rev().copied().collect();
    for i in kids {
        let mut kid = node.kids.remove(&i).unwrap();
        path.push(i);
        settle(&mut kid, path, replaced, w, gone, head)?;
        path.pop();
        gone.push(kid.at);
        let mut parents = Vec::new();
        for page in pack(kid.entries)? {
            let at = w.place(&encode_page(&page))?;
            parents.push(Entry::over(&page, at, &head.bound_slots)?);
        }
        node.entries.splice(i..i + 1, parents);
    }
    if node.height == 1 {
        let here: Vec<(usize, &Vec<Entry>)> = replaced
            .iter()
            .filter(|(p, _)| p.len() == path.len() + 1 && p[..path.len()] == path[..])
            .map(|(p, e)| (p[path.len()], e))
            .collect();
        for (i, entries) in here.into_iter().rev() {
            node.entries.splice(i..i + 1, entries.iter().cloned());
        }
    }
    Ok(())
}
