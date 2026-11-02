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
    decode_free, decode_sequences, encode_free, encode_sequences, Extent, Place, Root, Run, Writer,
    FORMAT_SEVEN, STAGED,
};
use crate::lock;

/// The runs of reused pages the root names, at most.
const REUSED_KEPT: usize = 64;

/// What a transaction answers once one of its statements failed, until it ends, as PostgreSQL does.
pub(super) const ABORTED: &str =
    "current transaction is aborted, commands ignored until end of transaction block";
use crate::places::{byte_key, key, month_of, null_key, KeyValue};
use crate::values::Values;
use crate::write::{empty_head, head_of, laid_out, leaf_descriptors, write_block, Laid, Rows};
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
    /// The orders it wrote: those holding a column it changed.
    pub orders: u64,
    /// Of the pages written, those written into pages freed before.
    pub pages_reused: u64,
    /// A question's answer.
    pub answer: Option<RecordBatch>,
}

/// How a writer takes a block's lock: waiting while another writer has it, as long as it takes,
/// unless the writers waiting make a ring; or at once or not at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Waiting {
    Wait,
    Try,
}

/// One row's change on a table.
#[derive(Clone)]
pub(super) enum Change {
    Insert(Row),
    Delete(Row),
    Replace(Row, Row),
}

/// One row's change on one order: its key there, the values the order holds, and for a row
/// replaced, the key of the row that replaced it.
struct Effect {
    key: Vec<KeyValue>,
    values: Vec<Datum>,
    delete: bool,
    chain: Option<Vec<KeyValue>>,
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

/// What a statement under way had staged when it began, and what its foreign keys had recorded.
struct Mark {
    rebased: BTreeMap<(usize, usize), Vec<Rebased>>,
    staged: u64,
    touched: Touched,
    pending: Touched,
    modes: BTreeMap<(usize, usize), bool>,
}

/// The catalog a statement or a transaction declared: the catalog it began from, its DDL now, and
/// the orders it made, each empty until rows come.
pub(super) struct Declared {
    from: Extent,
    ddl: String,
    made: Vec<(String, String)>,
    /// The sequences it made or restarted, which stand where the DDL leaves them.
    resets: Vec<String>,
}

/// A directory page read on the way down, and the pages under it read too.
struct Node {
    entries: Vec<Entry>,
    height: u32,
    kids: BTreeMap<usize, Node>,
    /// The page it was read from; none at the top.
    at: Extent,
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
fn level_key(
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
        Datum::Bytes(b) => byte_key(ty, b),
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
fn stated_types(catalog: &Catalog, question: &str) -> Vec<Option<ColumnType>> {
    use sqlparser::ast::{SelectItem, SetExpr, Statement as Sql, TableFactor};
    let dialect = sqlparser::dialect::PostgreSqlDialect {};
    let Ok(parsed) = sqlparser::parser::Parser::parse_sql(&dialect, question) else {
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
    let parsed =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, find)?;
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
/// question gives it, or where none is known fills only NULLs.
fn assigned_types(
    catalog: &Catalog,
    question: &str,
    batch: &RecordBatch,
    ti: usize,
    fills: &[(usize, usize)],
) -> Result<Vec<ColumnType>> {
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
    let stated = stated_types(catalog, question);
    let t = &catalog.tables[ti];
    for &(c, i) in fills {
        let col = &t.columns[c];
        let typed = batch.column(i).null_count() < batch.num_rows();
        let from = if typed {
            Some(types[i])
        } else {
            stated.get(i).copied().flatten()
        };
        if let Some(from) = from.filter(|&from| !assignable(from, col.ty)) {
            return Err(Error::new(format!(
                "column \"{}\" is of type {} but expression is of type {}",
                col.name,
                col.ty.pg_name(),
                from.pg_name().split('(').next().unwrap_or_default()
            )));
        }
    }
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

/// The sequence a column's DEFAULT draws from: its identity's, or its DEFAULT's nextval.
fn sequence_of(catalog: &Catalog, col: &crate::catalog::Column) -> Option<String> {
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
    /// answers it, its answer in what it did.
    pub fn execute_with(&mut self, sql: &str, params: &[String]) -> Result<Done> {
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
        let _handed = super::handed::holding(&self.functions);
        let _clocked = crate::scalars::clocked(self.clock.0, self.clock.1);
        let written_out;
        let text = match params.is_empty() {
            true => sql,
            false => {
                written_out = statement::with_params(sql, params)?;
                written_out.as_str()
            }
        };
        let st = statement::parse(&self.catalog, text)?;
        // A transaction one of whose statements failed takes no statement but its end, which rolls
        // it back, as PostgreSQL does.
        if self.aborted {
            return match st {
                Statement::Commit | Statement::Rollback => {
                    self.begun = None;
                    self.aborted = false;
                    self.levels.end(false);
                    Ok(Done {
                        statement: "ROLLBACK",
                        generation: self.root.generation,
                        ..Default::default()
                    })
                }
                _ => Err(Error::new(ABORTED)),
            };
        }
        if let Statement::Question { .. } = st {
            return self.answered(sql, params);
        }
        // A transaction READ ONLY takes no write, in PostgreSQL's words.
        let begun = self.begun.is_some();
        if self.levels.read_only(begun) {
            let what = match &st {
                Statement::Insert { .. } => Some("INSERT"),
                Statement::Update { .. } | Statement::Set { .. } => Some("UPDATE"),
                Statement::Delete { .. } => Some("DELETE"),
                Statement::Declare { kind, .. } => Some(*kind),
                _ => None,
            };
            if let Some(what) = what {
                if begun {
                    self.abandon()?;
                    self.aborted = true;
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
        if snapshot && self.levels.serializable(begun) {
            if let Err(e) = self.serial_snapshot(begun) {
                self.abandon()?;
                self.aborted = begun;
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
        self.rounds += 1;
        let (read0, written0, reused0) = (self.reader.pages_read(), self.written(), self.reused);
        let mut done = Done {
            statement: st.kind(),
            ..Default::default()
        };
        let changed = match st {
            Statement::Setting { .. } => false,
            Statement::Begin
            | Statement::Isolation {
                at: statement::Isolated::Begin,
                ..
            } => {
                if self.begun.is_some() {
                    return Err(Error::new("a transaction is begun already"));
                }
                let modes = match st {
                    Statement::Isolation { modes, .. } => modes,
                    _ => Default::default(),
                };
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
                .and_then(|_| self.levels.set(modes, at, begun));
                if let Err(e) = set {
                    self.aborted = begun;
                    return Err(e);
                }
                false
            }
            Statement::Commit => {
                if self.begun.is_none() {
                    return Err(Error::new("no transaction is begun"));
                }
                // The foreign keys deferred to the commit are checked on the newest root; one that
                // fails rolls the transaction back, as PostgreSQL's COMMIT does.
                let checked = match self.pending.is_empty() {
                    true => Ok(()),
                    false => self
                        .renew_in_transaction()
                        .and_then(|_| self.keys_at_commit()),
                };
                if let Err(e) = checked {
                    let (began, _) = self.begun.take().expect("a transaction begun");
                    self.root = began;
                    self.levels.end(false);
                    self.abandon()?;
                    return Err(e);
                }
                let (_, changed) = self.begun.take().expect("a transaction begun");
                self.levels.end(true);
                changed
            }
            Statement::Rollback => {
                let (root, _) = self
                    .begun
                    .take()
                    .ok_or_else(|| Error::new("no transaction is begun"))?;
                self.root = root;
                self.levels.end(false);
                self.abandon()?;
                false
            }
            st => {
                // A snail marked a replica takes writes from the applier presenting its source
                // alone.
                if !matches!(st, Statement::Constraints { .. }) {
                    if let Err(e) = self.writes_marked(&self.root) {
                        self.abandon()?;
                        self.aborted = self.begun.is_some();
                        return Err(e);
                    }
                }
                // A statement that reads and writes takes the transaction's first snapshot; SET
                // CONSTRAINTS takes none.
                if self.begun.is_some() && !matches!(st, Statement::Constraints { .. }) {
                    self.levels.read();
                }
                self.touched = Touched::default();
                let mark = self.mark();
                // The rows the statement found when it began, which it keeps to if it runs again.
                let mut found = None;
                loop {
                    self.stale = false;
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
                    match written {
                        Ok(_) if self.stale => {
                            // A block the statement locked was written since the root it read: it
                            // runs again on the newest root, kept to the rows it found, each in its
                            // newest version and still meeting its conditions, as PostgreSQL checks
                            // a row changed meanwhile again. The blocks it locked stay locked.
                            // At REPEATABLE READ it finds its rows again on the newest root too, read
                            // beside the generation the transaction holds, and is refused; a row
                            // written goes into a block it cannot read beside that generation.
                            self.back_to(&mark);
                            if self.held {
                                if matches!(st, Statement::Insert { .. }) {
                                    self.abandon()?;
                                    self.aborted = self.begun.is_some();
                                    return Err(Error::new(
                                        "could not serialize access due to concurrent update",
                                    ));
                                }
                                let newest = self.reader.peek_root()?;
                                self.in_transaction_on(newest)?;
                            } else if self.begun.is_some() {
                                self.renew_in_transaction()?;
                            } else {
                                self.renew()?;
                            }
                        }
                        Ok(changed) => break changed,
                        Err(e) => {
                            self.abandon()?;
                            self.aborted = self.begun.is_some();
                            return Err(e);
                        }
                    }
                }
            }
        };
        if let Some((_, any)) = self.begun.as_mut() {
            *any |= changed;
        } else if changed || self.serial_records()? {
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
                    self.abandon()?;
                    self.aborted = true;
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
            self.abandon()?;
            self.aborted = true;
        }
        marked
    }

    fn marked(&mut self, source: Option<&str>) -> Result<Done> {
        if source == Some("") {
            return Err(Error::new("a replica's source is named: the name is empty"));
        }
        if source.is_some() && self.reader.format() < crate::file::FORMAT {
            return Err(Error::new(format!(
                "a snail of format {} is marked a replica of no source: `out`, then `write`",
                self.reader.format()
            )));
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
            Statement::Insert { table, source } => {
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
                    (Source::Select { question, fill }, None) => {
                        let (batch, _) = self.answer(&question, None)?;
                        let fills: Vec<(usize, usize)> = fill
                            .iter()
                            .enumerate()
                            .filter_map(|(c, f)| match f {
                                Fill::Answer(i) => Some((c, *i)),
                                _ => None,
                            })
                            .collect();
                        let types =
                            assigned_types(&self.catalog, &question, &batch, table, &fills)?;
                        let t = &self.catalog.tables[table];
                        let mut given = Vec::new();
                        for a in rows_of(&batch)? {
                            let mut row = Vec::with_capacity(fill.len());
                            for (col, f) in t.columns.iter().zip(&fill) {
                                row.push(match f {
                                    Fill::Answer(i) => Fill::Literal(fill::assign(
                                        a[*i].clone(),
                                        types[*i],
                                        col.ty,
                                        &col.name,
                                    )?),
                                    other => other.clone(),
                                });
                            }
                            given.push(row);
                        }
                        self.filled(table, given)?
                    }
                };
                *found = Some(rows.clone());
                done.rows = rows.len() as u64;
                (done.rewritten, done.orders) =
                    self.apply(table, rows.into_iter().map(Change::Insert).collect())?;
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
            Statement::Delete { table, find } => {
                let (batch, _) = self.answer(&find, None)?;
                let t = &self.catalog.tables[table];
                let old = self.kept_to(table, widen(t, rows_of(&batch)?), found)?;
                done.rows = old.len() as u64;
                (done.rewritten, done.orders) =
                    self.apply(table, old.into_iter().map(Change::Delete).collect())?;
                done.rows > 0
            }
            Statement::Declare { ddl, .. } => {
                self.declare(&ddl)?;
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
            | Statement::Question { .. } => {
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
        let mut computed: Vec<(usize, sqlparser::ast::Expr)> = Vec::new();
        for (c, f) in &sets {
            if let Fill::Expression(text) = f {
                let e = sqlparser::parser::Parser::new(&sqlparser::dialect::PostgreSqlDialect {})
                    .try_with_sql(text)?
                    .parse_expr()?;
                computed.push((*c, e));
            }
        }
        if !computed.is_empty() {
            let typing = format!(
                "SELECT {} FROM {relation}",
                computed
                    .iter()
                    .map(|(_, e)| format!("({e})"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            for ((c, _), from) in computed.iter().zip(stated_types(&self.catalog, &typing)) {
                let col = &t.columns[*c];
                if let Some(from) = from.filter(|&from| !assignable(from, col.ty)) {
                    return Err(Error::new(format!(
                        "column \"{}\" is of type {} but expression is of type {}",
                        col.name,
                        col.ty.pg_name(),
                        from.pg_name().split('(').next().unwrap_or_default()
                    )));
                }
            }
        }
        let (batch, _) = self.answer(&find, None)?;
        let old = self.kept_to(table, widen(&t, rows_of(&batch)?), found)?;
        done.rows = old.len() as u64;
        let defaults: Vec<usize> = sets
            .iter()
            .filter(|(_, f)| matches!(f, Fill::Default))
            .map(|(c, _)| *c)
            .collect();
        let mut drawn = self.draws(&t, defaults.iter().map(|&c| (c, old.len())))?;
        let mut changes = Vec::with_capacity(old.len());
        for o in old {
            let mut n = o.clone();
            for (c, f) in &sets {
                n[*c] = match f {
                    Fill::Literal(d) => d.clone(),
                    Fill::Expression(_) => {
                        let e = &computed
                            .iter()
                            .find(|(k, _)| k == c)
                            .expect("an expression")
                            .1;
                        fill::set_to(&t, &o, *c, e, &by)?
                    }
                    _ => self.default_of(&t, *c, &mut drawn)?,
                };
            }
            fill::generate(&t, &mut n)?;
            fill::check_domains(&self.catalog.domains, &t, &n)?;
            fill::check(&t, &n)?;
            fill::without_overlaps(&t, &n)?;
            changes.push(Change::Replace(o, n));
        }
        (done.rewritten, done.orders) = self.apply(table, changes)?;
        Ok(done.rows > 0)
    }

    /// The rows an INSERT gives, each cell a value or its column's DEFAULT: the DEFAULTs filled,
    /// a sequence's values drawn once for the statement, the stored generated columns computed and
    /// the CHECKs checked. A value given to a generated column, or to an identity GENERATED ALWAYS,
    /// is refused, as PostgreSQL refuses it.
    fn filled(&mut self, table: usize, rows: Vec<Vec<Fill>>) -> Result<Vec<Row>> {
        let t = self.catalog.tables[table].clone();
        for r in &rows {
            for (col, f) in t.columns.iter().zip(r) {
                let always = col.identity.as_ref().is_some_and(|i| i.always);
                if (col.generated.is_some() || always) && !matches!(f, Fill::Default) {
                    return Err(Error::new(format!(
                        "cannot insert a non-DEFAULT value into column \"{}\"",
                        col.name
                    )));
                }
            }
        }
        let counts = (0..t.columns.len()).map(|c| {
            (
                c,
                rows.iter()
                    .filter(|r| matches!(r[c], Fill::Default))
                    .count(),
            )
        });
        let mut drawn = self.draws(&t, counts)?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let mut row = Vec::with_capacity(r.len());
            for (c, f) in r.into_iter().enumerate() {
                row.push(match f {
                    Fill::Literal(d) => d,
                    _ => self.default_of(&t, c, &mut drawn)?,
                });
            }
            fill::generate(&t, &mut row)?;
            fill::check_domains(&self.catalog.domains, &t, &row)?;
            fill::check(&t, &row)?;
            fill::without_overlaps(&t, &row)?;
            out.push(row);
        }
        Ok(out)
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
            let values = self.draw(&s, n)?;
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
        fill::assign_value(
            super::pipeline::eval_on(
                e,
                &[],
                &[],
                t.release,
                &super::pipeline::Casings::under(t.monetary),
            )?,
            col.ty,
            &col.name,
        )
    }

    /// The values a sequence hands this writer: those its cache holds first, then a run of CACHE
    /// values at a time drawn at the root's turn, never given back.
    fn draw(&mut self, name: &str, n: usize) -> Result<Vec<i64>> {
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
        if out.len() < n {
            let missing = n - out.len();
            let cache = s.cache.max(1) as usize;
            let chunk = missing.div_ceil(cache) * cache;
            let fetched = self.advance(&s, chunk, missing)?;
            out.extend_from_slice(&fetched[..missing]);
            self.handed
                .entry(name.to_string())
                .or_default()
                .extend(fetched[missing..].iter().copied());
        }
        Ok(out)
    }

    /// Advances a sequence by up to `chunk` values at the root's turn, a commit of its own, as
    /// PostgreSQL's nextval is never rolled back: at least `need` of them, or refused where a
    /// sequence that does not cycle runs out first.
    fn advance(
        &mut self,
        s: &crate::catalog::Sequence,
        chunk: usize,
        need: usize,
    ) -> Result<Vec<i64>> {
        let mut w = self
            .writer
            .take()
            .ok_or_else(|| Error::new("this snail was opened to read; open it to write"))?;
        let turn = lock::take_root_turn(w.file());
        let drawn = turn.and_then(|_| {
            let root = self.reader.peek_root()?;
            let mut states = if root.sequences.bytes > 0 {
                decode_sequences(&self.reader.read_extent(root.sequences)?)?
            } else {
                Vec::new()
            };
            let (mut last, mut called) = states
                .iter()
                .find(|x| x.0 == s.name)
                .map(|x| (x.1, x.2))
                .unwrap_or((s.last_value, s.is_called));
            let mut out = Vec::with_capacity(chunk);
            while out.len() < chunk {
                let next = if called {
                    last.checked_add(s.increment)
                        .filter(|v| *v >= s.min && *v <= s.max)
                } else {
                    Some(last)
                };
                match next {
                    Some(v) => last = v,
                    None if out.len() >= need => break,
                    None if s.cycle => last = if s.increment > 0 { s.min } else { s.max },
                    None => {
                        let (end, bound) = if s.increment > 0 {
                            ("maximum", s.max)
                        } else {
                            ("minimum", s.min)
                        };
                        return Err(Error::new(format!(
                            "nextval: reached {end} value of sequence \"{}\" ({bound})",
                            s.name.rsplit('.').next().unwrap_or(&s.name)
                        )));
                    }
                }
                called = true;
                out.push(last);
            }
            states.retain(|x| x.0 != s.name);
            states.push((s.name.clone(), last, true));
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
        let out = drawn?;
        given?;
        Ok(out)
    }

    /// The rows an UPDATE or a DELETE finds: the first time, every one, kept as `found`; run again,
    /// only those whose key, on the table's first unique order of plain NOT NULL columns or else
    /// every column, is a key it found the first time.
    fn kept_to(&self, ti: usize, now: Vec<Row>, found: &mut Option<Vec<Row>>) -> Result<Vec<Row>> {
        let Some(first) = found.as_ref() else {
            *found = Some(now.clone());
            return Ok(now);
        };
        let table = &self.catalog.tables[ti];
        let columns: Vec<usize> = table
            .orders
            .iter()
            .filter(|o| o.unique)
            .find_map(|o| {
                o.levels
                    .iter()
                    .map(|l| match l {
                        Level::Column(c) if table.columns[*c].not_null => Some(*c),
                        _ => None,
                    })
                    .collect::<Option<Vec<usize>>>()
            })
            .unwrap_or_else(|| (0..table.columns.len()).collect());
        let key = |r: &Row| -> Vec<Datum> { columns.iter().map(|&c| r[c].clone()).collect() };
        // At REPEATABLE READ a row found that another transaction changed or deleted since the
        // transaction's generation is refused, in PostgreSQL's words, SQLSTATE 40001.
        if self.levels.repeatable(self.begun.is_some()) {
            let width = table.columns.len();
            let newest: std::collections::HashMap<Vec<Datum>, &Row> =
                now.iter().map(|r| (key(r), r)).collect();
            for f in first {
                match newest.get(&key(f)) {
                    None => {
                        return Err(Error::new(
                            "could not serialize access due to concurrent delete",
                        ))
                    }
                    Some(r) if r[..width] != f[..width] => {
                        return Err(Error::new(
                            "could not serialize access due to concurrent update",
                        ))
                    }
                    Some(_) => {}
                }
            }
            // In a transaction holding its generation, a block another wrote since is refused too:
            // its rows would be read from two generations.
            if self.held {
                return Err(Error::new(
                    "could not serialize access due to concurrent update",
                ));
            }
        }
        let keys: std::collections::HashSet<Vec<Datum>> = first.iter().map(key).collect();
        Ok(now.into_iter().filter(|r| keys.contains(&key(r))).collect())
    }

    /// Where the statement under way began: what it had staged and the root it read, so that it
    /// can run again from there.
    fn mark(&self) -> Mark {
        Mark {
            rebased: self.rebased.clone(),
            staged: self.reader.staged_mark(),
            touched: self.touched.clone(),
            pending: self.pending.clone(),
            modes: self.modes.clone(),
        }
    }

    fn back_to(&mut self, mark: &Mark) {
        self.rebased = mark.rebased.clone();
        self.reader.staged_back_to(mark.staged);
        self.touched = mark.touched.clone();
        self.pending = mark.pending.clone();
        self.modes = mark.modes.clone();
    }

    /// What a statement of a transaction reads: the newest root, with the transaction's own changes
    /// in place of the blocks it changed, each still the block it began from, since the
    /// transaction holds its lock. A transaction that declared a table reads as it began. Returns
    /// the pages it read.
    pub(super) fn renew_in_transaction(&mut self) -> Result<u64> {
        if self.declared.is_some() {
            return Ok(0);
        }
        let before = self.reader.pages_read();
        let root = self.reader.begin_round()?;
        self.in_transaction_on(root)?;
        Ok(self.reader.pages_read() - before)
    }

    /// What a statement of a transaction reads on `root`: the transaction's own changes in place of
    /// the blocks it changed.
    pub(super) fn in_transaction_on(&mut self, mut root: Root) -> Result<()> {
        if root.generation == self.root.generation {
            return Ok(());
        }
        self.catalog = Snail::catalog(&self.reader, &root)?;
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
                head.separated,
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
        self.deciding = None;
        self.marking = None;
        self.reader.clear_staged();
        self.touched = Touched::default();
        self.pending = Touched::default();
        self.modes.clear();
        if self.declared.take().is_some() {
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
        self.unnamed.clear();
        self.held = false;
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
        let file = self.writer.as_ref().expect("a snail open to write").file();
        if !lock::lock_as(file, b, shared, false)? {
            if waiting == Waiting::Try {
                return Ok(false);
            }
            lock::mark_waiting(file, self.slot, b, true)?;
            let (group, me) = self.group.as_ref().expect("a snail open to write");
            let ring = group.ring(*me, b);
            if !matches!(ring, Ok(false)) {
                lock::mark_waiting(file, self.slot, b, false)?;
                ring?;
                return Err(Error::new(
                    "deadlock detected: this writer waits for a block held by a writer that waits, \
                     in a ring, for a block this writer holds",
                ));
            }
            let taken = lock::lock_as(file, b, shared, true);
            lock::mark_waiting(file, self.slot, b, false)?;
            taken?;
        }
        lock::mark_held(file, self.slot, b, true)?;
        Ok(true)
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
            .and_then(|_| self.serial_tested(w.file()))
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
        // A transaction's root carries its host's tag; a sequence's advance lands at once.
        let advance = self.advancing.is_some();
        let mut root = self.commit_pages(w, &[])?;
        root.host = match w.host_tag() {
            Some(_) if advance => Some(crate::carry::AT_ONCE),
            tag => tag,
        };
        // The root goes into the copy that does not hold the root it begins from.
        w.commit_into(&mut root, 1 - self.reader.root_copy())?;
        Ok(root)
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
        // commits it once, besides the advances of its sequences.
        if !self.reader.judged() && (root.host.is_some() || self.reader.waiting_on().is_some()) {
            return Err(Error::new(
                "this snail is written through its host, which decides which of its roots is \
                 taken: write it through its host",
            ));
        }
        if let Some(t) = w.host_tag().filter(|&t| t != crate::carry::AT_ONCE) {
            let lies = root.host == Some(t) || self.reader.waiting_on() == Some(t);
            if lies && self.advancing.is_none() {
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
        if self.advancing.is_none() {
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
        let generation = root.generation + 1;
        // The free map: its runs whose tag no older round still runs under are free now, and the
        // commit takes pages from the free runs first.
        let (mut runs, mut spent) = if root.free.bytes > 0 {
            decode_free(&self.reader.read_extent(root.free)?, w.format())?
        } else {
            (Vec::new(), Vec::new())
        };
        let mut maps = MapPages::new(w.format() != FORMAT_SEVEN);
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
        let mut freed = 0;
        for t in tags {
            if lock::older_held(self.reader.file(), t)? {
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
        // The catalog declared: written afresh, each table it touches given the orders it declares,
        // those it made empty.
        if let Some(d) = self.declared.take() {
            if root.catalog != d.from {
                return Err(Error::new(
                    "the catalog was declared since by another writer",
                ));
            }
            gone.push(root.catalog);
            root.catalog = w.place(d.ddl.as_bytes())?;
            root.monetary = crate::file::monetary_of(&self.catalog, &root.monetary);
            if !d.resets.is_empty() && root.sequences.bytes > 0 {
                let mut states = decode_sequences(&self.reader.read_extent(root.sequences)?)?;
                states.retain(|s| !d.resets.contains(&s.0));
                gone.push(root.sequences);
                root.sequences = w.place(&encode_sequences(&states))?;
            }
            let touched: Vec<&String> = d.made.iter().map(|(t, _)| t).collect();
            let mut orders = Vec::new();
            for (t, o, e) in std::mem::take(&mut root.orders) {
                let made = d.made.contains(&(t.clone(), o.clone()));
                let declared = self
                    .catalog
                    .table(&t)
                    .is_some_and(|x| x.orders.iter().any(|y| y.name == o));
                if touched.contains(&&t) && (made || !declared) {
                    gone.push(e);
                } else {
                    orders.push((t, o, e));
                }
            }
            for (tn, on) in &d.made {
                if let Some(t) = self.catalog.table(tn) {
                    if let Some(o) = t.orders.iter().find(|x| x.name == *on) {
                        let top =
                            w.place(&encode_top(&empty_head(t, o, self.reader.format()), &[]))?;
                        orders.push((tn.clone(), on.clone(), top));
                    }
                }
            }
            root.orders = orders;
        }
        // The sequences a statement advanced, at the root's turn, each where it stands now.
        if let Some(states) = self.advancing.take() {
            if root.sequences.bytes > 0 {
                gone.push(root.sequences);
            }
            root.sequences = w.place(&encode_sequences(&states))?;
        }
        // A transaction across snails this commit decides: the roots it writes into the others,
        // carried.
        if let Some((roots, bytes)) = self.deciding.take() {
            let carried = w.place(&bytes)?;
            root.decided.push(crate::file::Decision {
                at: generation,
                roots,
                carried,
            });
        }
        // The serializable transactions' records, and what the host's monitor is told of its cuts.
        self.tell_cuts()?;
        self.records_at_commit(w, &mut root, generation, &mut gone)?;
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
            settle(&mut top, &mut Vec::new(), &replaced, w, &mut gone, head.separated)?;
            gone.push(root.orders[at].2);
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            root.orders[at].2 = w.place(&bytes)?;
        }
        maps.finish(w)?;
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
                tag: generation,
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
            w.place_end(&encode_free(&runs, &spent, w.format()))?
        };
        for (start, pages) in w.taken() {
            self.reused += pages;
            root.reused.push((generation, start, pages));
        }
        while root.reused.len() > REUSED_KEPT {
            let (g, _, _) = root.reused.remove(0);
            root.reused_from = root.reused_from.max(g);
        }
        root.generation = generation;
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
                    for (ci, (col, d)) in table.columns.iter().zip(r).enumerate() {
                        if let ColumnType::Numeric { precision, .. } = col.ty {
                            if d.wide() == Some(crate::numeric::nan_units(precision)) {
                                crate::file::holds_nan(self.reader.format(), &table, ci)?;
                            }
                        }
                        if col.ty.is_timestamptz() && matches!(d, Datum::Int(i64::MAX | i64::MIN)) {
                            crate::file::instant_ends(self.reader.format(), &table, ci)?;
                        }
                    }
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
        self.name_rows(ti, &named)?;
        let mut stage = self.reader.stage();
        let applied: Vec<u64> = (0..table.orders.len())
            .map(|oi| self.apply_order(&mut stage, ti, &table, oi, &changes))
            .collect::<Result<Vec<Option<u64>>>>()?
            .into_iter()
            .flatten()
            .collect();
        if !self.stale {
            self.touch(ti, &changes);
            self.touch_exclusions(ti, &changes);
        }
        Ok((applied.iter().sum(), applied.len() as u64))
    }

    /// One order's part of a statement: its effects routed to their blocks, each block written
    /// afresh, then the directory above them up to a new top. Returns the blocks rewritten in order,
    /// or none where the order holds no column the statement changed.
    fn apply_order(
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
        let held =
            |r: &Row| -> Vec<Datum> { order.columns.iter().map(|&c| r[c].clone()).collect() };
        // A partial strand takes the rows its predicate admits: a row replaced by one it does not
        // admit leaves it, and one it did not admit comes in when its replacement passes.
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
        let mut effects = Vec::new();
        for c in changes {
            match c {
                Change::Insert(r) => effects.push(Effect {
                    key: key_of(table, order, r)?,
                    values: held(r),
                    delete: false,
                    chain: None,
                }),
                Change::Delete(r) => effects.push(Effect {
                    key: key_of(table, order, r)?,
                    values: held(r),
                    delete: true,
                    chain: None,
                }),
                Change::Replace(o, n) => {
                    if held(o) == held(n) {
                        continue;
                    }
                    let to = key_of(table, order, n)?;
                    effects.push(Effect {
                        key: key_of(table, order, o)?,
                        values: held(o),
                        delete: true,
                        chain: Some(to.clone()),
                    });
                    effects.push(Effect {
                        key: to,
                        values: held(n),
                        delete: false,
                        chain: None,
                    });
                }
            }
        }
        if effects.is_empty() {
            return Ok(None);
        }
        if effects.iter().any(|e| e.key.iter().any(KeyValue::is_null)) {
            crate::file::keys_null(self.reader.format(), table, order)?;
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
            let b = lock::block_byte_of(ti, oi, &[]);
            if !self.locks.contains(&b) {
                self.lock_byte(b, Waiting::Wait)?;
                self.locks.insert(b);
                if !self.empty_in_newest(table, order)? {
                    self.stale = true;
                    return Ok(None);
                }
            }
            self.mark_wrote(ti, oi, &[b])?;
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
            let bytes: Vec<u64> = blocks
                .iter()
                .filter(|e| e.child.start < STAGED)
                .map(|e| lock::block_byte_of(ti, oi, &e.first))
                .collect();
            if !self.lock_blocks(ti, oi, blocks, Waiting::Wait)? {
                self.stale = true;
                return Ok(None);
            }
            self.mark_wrote(ti, oi, &bytes)?;
            for (path, effects) in by_block {
                let entry = entry_at(&top, &path).clone();
                let (new, rewrote) =
                    self.write_block_afresh(w, table, order, &head, &entry, effects, false)?;
                pruned += rewrote as u64;
                self.rebase((ti, oi), &entry, &new)?;
                replaced.insert(path, new);
            }
        }
        let mut path = Vec::new();
        settle(&mut top, &mut path, &replaced, w, &mut Vec::new(), head.separated)?;
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
        let mut want: Vec<(u64, Entry)> = blocks
            .into_iter()
            .filter(|e| e.child.start < STAGED)
            .map(|e| (lock::block_byte_of(ti, oi, &e.first), e))
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
    /// transaction staged lies under the lock of the block it began from, and is its own.
    pub(super) fn newest_names(&self, ti: usize, oi: usize, blocks: &[Entry]) -> Result<bool> {
        let blocks: Vec<&Entry> = blocks.iter().filter(|e| e.child.start < STAGED).collect();
        let root = self.reader.peek_root()?;
        let table = &self.catalog.tables[ti];
        let order = &table.orders[oi];
        let at = root
            .orders
            .iter()
            .position(|(t, o, _)| *t == table.name && *o == order.name)
            .ok_or_else(|| {
                Error::new(format!("{}: the root names no {}", table.name, order.name))
            })?;
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
    fn locate(
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

    /// Prunes the blocks a round found a page or more of waste in: each rewritten in order, its dead
    /// rows dropped, and cut again; then committed as a write is, or within the transaction begun.
    /// Returns the blocks pruned and the pages written.
    pub(super) fn prune(&mut self, wasted: Vec<Wasted>) -> Result<(u64, u64)> {
        // A snail marked a replica is pruned by its applier's reads alone.
        if self.writes_marked(&self.root).is_err() {
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
                    .contains(&lock::block_byte_of(ti, oi, &entry.first));
                if self.begun.is_some() && !held {
                    continue;
                }
                if !self.lock_blocks(ti, oi, vec![entry.clone()], Waiting::Try)? {
                    continue;
                }
                let (new, _) =
                    self.write_block_afresh(w, &table, order, &head, &entry, Vec::new(), true)?;
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
                head.separated,
            )?;
            let (bytes, _) = build_from(head, top.entries, top.height, &mut |x| w.place(x))?;
            self.root.orders[at].2 = w.place(&bytes)?;
        }
        Ok(pruned)
    }

    /// One block's effects written: the dead marked and the new rows written after each column's
    /// last rows; or the block rewritten in order, where `rewrite` asks it, where it would hold more
    /// rows than a row group, or where rows come and the cut of its rows would make it more than one
    /// block. Returns the entries that stand for it now, and whether it was rewritten.
    #[allow(clippy::too_many_arguments)]
    fn write_block_afresh(
        &self,
        w: &mut dyn Place,
        table: &Table,
        order: &Order,
        head: &Head,
        entry: &Entry,
        effects: Vec<Effect>,
        rewrite: bool,
    ) -> Result<(Vec<Entry>, bool)> {
        let mut map = read_map(&self.reader, entry, &head.level_text)?;
        let positions = map.rows() as usize;
        let leaves = order.leaves(table);
        let descs = leaf_descriptors(table, order)?;
        // Every column of the block, whole: each of its leaves decoded, and the column made of them.
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
        let mut dead: std::collections::BTreeSet<u32> = map.dead.iter().copied().collect();
        let types: Vec<ColumnType> = order.columns.iter().map(|&c| table.columns[c].ty).collect();
        let holds = |p: usize, values: &[Datum]| -> bool {
            columns
                .iter()
                .zip(values)
                .zip(&types)
                .all(|((v, d), &ty)| datum_typed(v, p, ty) == *d)
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
        let split = !added.is_empty() && {
            let r = rows_now();
            let cut = laid_out(
                table,
                order,
                &r,
                Some(&head.level_text),
                self.reader.format(),
            )?
            .blocks
            .len()
                > 1;
            rows = Some(r);
            cut
        };
        if rewrite || split || positions + added.len() > ROW_GROUP_ROWS {
            // Written in order, and cut by the counts as a block written afresh is.
            let rows = rows.unwrap_or_else(rows_now);
            let mut written = self.blocks_written(w, table, order, head, &rows)?;
            if head.separated {
                crate::directory::separate(
                    &mut written,
                    &head.level_text,
                    Some(&entry.first),
                    Some(&entry.last),
                );
            }
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
                let k = if cp.unit != 0 { cp.unit } else { head.units[s] };
                for p in tail_pages(&descs[s], ty, &values, k as usize)?.pages {
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
            crate::directory::widened(&mut entry, key, head.separated);
        }
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
            // A column keyed by its bytes widens its bounds, kept in its first leaf's, by each row added.
            if head.bound_slots.contains(&(s as u32)) {
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
                    if let Some(v) = key.map(|k| crate::directory::prefix_bound(&k)) {
                        if first_value {
                            (sum.min, sum.max) = (v, v);
                            first_value = false;
                        } else {
                            (sum.min, sum.max) = (sum.min.min(v), sum.max.max(v));
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
        } = laid_out(
            table,
            order,
            rows,
            Some(&head.level_text),
            self.reader.format(),
        )?;
        let descs = leaf_descriptors(table, order)?;
        // In format 9 each block takes the unit of its own furthest-out place.
        let kept = self.reader.format() >= crate::file::FORMAT;
        let block_units: Vec<Vec<usize>> = if kept {
            crate::cut::largest_places(&keys, &blocks, order, table, &widths)
                .iter()
                .map(|m| {
                    (0..widths.0.len())
                        .map(|s| widths.unit(s, m[s], rows.len()))
                        .collect()
                })
                .collect()
        } else {
            vec![head.units.iter().map(|&k| k as usize).collect(); blocks.len()]
        };
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
                kept,
            )?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// The rows an empty order is given, as its first blocks, and the head they give the order: its
    /// units and its text levels' sublevels, as a snail written from them would hold.
    fn written_in_order(
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
        let laid = laid_out(table, order, &rows, None, self.reader.format())?;
        let head = head_of(table, order, &laid, held.len(), self.reader.format());
        let mut written = self.blocks_written(w, table, order, &head, &rows)?;
        if head.separated {
            crate::directory::separate(&mut written, &head.level_text, None, None);
        }
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

    fn declare(&mut self, ddl: &str) -> Result<()> {
        let text = format!("{}\n{ddl};\n", self.catalog.ddl.trim_end());
        let kept = Catalog::statements_in(&self.catalog.ddl)?;
        let mut catalog = Catalog::parse_kept(&text, kept)?;
        super::old_names(&mut catalog, self.reader.format());
        catalog.with_release(self.root.release)?;
        catalog.with_monetary(&crate::file::monetary_of(&catalog, &self.root.monetary))?;
        let mut made: Vec<(String, String)> = Vec::new();
        let mut kept: Vec<(String, String)> = Vec::new();
        crate::file::holds_catalog(self.reader.format(), &catalog)?;
        for t in &catalog.tables {
            crate::file::holds(self.reader.format(), t)?;
            let old = self.catalog.tables.iter().find(|x| x.name == t.name);
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
                    });
                if same {
                    kept.push((t.name.clone(), o.name.clone()));
                } else {
                    made.push((t.name.clone(), o.name.clone()));
                    changed = true;
                }
            }
            if let (true, Some(o)) = (changed, old) {
                if self.holds_rows(o)? {
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
                return Ok(());
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
            let top = stage.place(&encode_top(&empty_head(t, o, self.reader.format()), &[]))?;
            orders.push((tn.clone(), on.clone(), top));
        }
        let from = self.declared.as_ref().map_or(self.root.catalog, |d| d.from);
        self.root.catalog = stage.place(text.as_bytes())?;
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
            .collect();
        self.catalog = catalog;
        let d = self.declared.get_or_insert(Declared {
            from,
            ddl: String::new(),
            made: Vec::new(),
            resets: Vec::new(),
        });
        for r in resets {
            if !d.resets.contains(&r) {
                d.resets.push(r);
            }
        }
        d.ddl = text;
        for m in made {
            if !d.made.contains(&m) {
                d.made.push(m);
            }
        }
        Ok(())
    }

    /// Whether any order of a table holds a live row, as the round reads the file.
    fn holds_rows(&self, table: &Table) -> Result<bool> {
        for (t, _, top) in &self.root.orders {
            if *t == table.name {
                let (_, entries) = decode_top(&self.reader.read_extent(*top)?)?;
                if entries.iter().any(|e| e.rows > 0) {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }
}

fn entry_at<'a>(node: &'a Node, path: &[usize]) -> &'a Entry {
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
fn settle(
    node: &mut Node,
    path: &mut Vec<usize>,
    replaced: &BTreeMap<Vec<usize>, Vec<Entry>>,
    w: &mut dyn Place,
    gone: &mut Vec<Extent>,
    separated: bool,
) -> Result<()> {
    let kids: Vec<usize> = node.kids.keys().rev().copied().collect();
    for i in kids {
        let mut kid = node.kids.remove(&i).unwrap();
        path.push(i);
        settle(&mut kid, path, replaced, w, gone, separated)?;
        path.pop();
        gone.push(kid.at);
        let mut parents = Vec::new();
        for page in pack(kid.entries, separated)? {
            let at = w.place(&encode_page(&page, separated))?;
            parents.push(Entry::over(&page, at)?);
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
