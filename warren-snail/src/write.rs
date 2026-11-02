//! A write from Arrow record batches: each row routed by its places onto the table's order and onto
//! each index, the blocks cut and encoded, every block's pages written, then the directory, then
//! the root.

use crate::catalog::{Catalog, ColumnType, Order, Table};
use crate::cut::{cut, largest_place, largest_places, Block, Width, Widths};
use crate::directory::{
    build, decode_top, encode_top, ColumnPages, Entry, Head, MapPages, PageEntry, PageMap, Summary,
    WIDE_LEVEL,
};
use crate::encode::{disk_pages, encode, leaf_descriptor, DataPage, Encoded, DICTIONARY_BYTES};
use crate::file::{
    encode_free, Extent, Place, Root, Run, Writer, FILE_PAGES, FORMAT_EIGHT, FORMAT_SEVEN,
};
use crate::places::{level_key, month_of, null_key, KeyValue, Keys, Sublevels};
use crate::values::Values;
use crate::{Error, Result};
use arrow::array::{
    Array, BinaryArray, BinaryViewArray, BooleanArray, FixedSizeBinaryArray, Int16Array,
    Int32Array, Int64Array, LargeBinaryArray, LargeStringArray, RecordBatch, StringArray,
    StringViewArray, TimestampMicrosecondArray,
};
use arrow::datatypes::{DataType, TimeUnit};
use parquet::data_type::ByteArray;
use std::path::Path;

/// A table's rows, column by column in the table's column order.
pub struct Rows {
    pub columns: Vec<Values>,
}

impl Rows {
    pub fn len(&self) -> usize {
        self.columns.first().map_or(0, Values::len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The table's columns, found by name in the batches.
    pub fn from_batches(table: &Table, batches: &[RecordBatch]) -> Result<Rows> {
        let mut columns = Vec::with_capacity(table.columns.len());
        let rows: usize = batches.iter().map(|b| b.num_rows()).sum();
        for c in &table.columns {
            let mut out = Values::new(c.ty);
            // A virtual column is computed at the read: no order holds it, and its rows give it
            // no value.
            if c.is_virtual() {
                if batches.iter().any(|b| b.column_by_name(&c.name).is_some()) {
                    return Err(Error::new(format!(
                        "{}.{} is a virtual generated column, and its rows give it no value",
                        table.name, c.name
                    )));
                }
                match &mut out {
                    Values::Int(x) => x.resize(rows, None),
                    Values::Text(x) => x.resize(rows, None),
                    Values::Utf8(x) => x.resize(rows, None),
                    Values::Wide(x) => x.resize(rows, None),
                    Values::Float4(x) => x.resize(rows, None),
                    Values::Float8(x) => x.resize(rows, None),
                    Values::Interval(x) => x.resize(rows, None),
                    _ => unreachable!("a column is read as plain values"),
                }
                columns.push(out);
                continue;
            }
            for b in batches {
                let a = b.column_by_name(&c.name).ok_or_else(|| {
                    Error::new(format!(
                        "{}: the rows have no column {}",
                        table.name, c.name
                    ))
                })?;
                let wrong = || {
                    Error::new(format!(
                        "{}.{}: {} given for {}",
                        table.name,
                        c.name,
                        a.data_type(),
                        c.ty.name()
                    ))
                };
                match (&mut out, c.ty, a.data_type()) {
                    // A composed column as arrow's Parquet reader gives its page form.
                    (Values::Composed(out), ty, _) => {
                        for r in 0..a.len() {
                            let v = crate::composed::compound_at(ty, a.as_ref(), r);
                            if v.is_none() && !a.is_null(r) {
                                return Err(wrong());
                            }
                            out.push(v);
                        }
                    }
                    // A path, a polygon or a snapshot, as arrow's Parquet reader gives its tree.
                    (out, ty, DataType::Struct(_)) if ty.is_held_tree() => {
                        for r in 0..a.len() {
                            let d =
                                crate::composed::group_at(ty, a.as_ref(), r).ok_or_else(wrong)?;
                            crate::values::push_datum(out, &d);
                        }
                    }
                    // An enum: its labels, as their ranks.
                    (
                        Values::Int(out),
                        ty @ ColumnType::Composed(_),
                        DataType::Binary | DataType::Utf8,
                    ) => {
                        for r in 0..a.len() {
                            let v = crate::composed::compound_at(ty, a.as_ref(), r);
                            if v.is_none() && !a.is_null(r) {
                                return Err(wrong());
                            }
                            out.push(v.and_then(|c| match c.item {
                                crate::composed::Item::Scalar(
                                    crate::read::statement::Datum::Int(k),
                                ) => Some(k),
                                _ => None,
                            }));
                        }
                    }
                    (Values::Wide(out), ColumnType::Uuid, DataType::FixedSizeBinary(16)) => {
                        let a = a.as_any().downcast_ref::<FixedSizeBinaryArray>().unwrap();
                        out.extend(
                            a.iter().map(|v| {
                                v.map(|b| crate::scalars::uuid_key(b.try_into().unwrap()))
                            }),
                        );
                    }
                    (Values::Int(out), ty, dt)
                        if crate::scalars::is_scalar(ty)
                            && *dt == crate::scalars::scalar_arrow(ty) =>
                    {
                        out.extend(
                            (0..a.len()).map(|r| crate::scalars::scalar_key_at(ty, a.as_ref(), r)),
                        );
                    }
                    // A time of day as a reader writes it in nanoseconds, each a whole microsecond.
                    (
                        Values::Int(out),
                        ColumnType::Time(_),
                        DataType::Time64(TimeUnit::Nanosecond),
                    ) => {
                        let a = a
                            .as_any()
                            .downcast_ref::<arrow::array::Time64NanosecondArray>()
                            .unwrap();
                        for v in a.iter() {
                            out.push(match v {
                                Some(ns) if ns % 1000 != 0 => {
                                    return Err(Error::new(format!(
                                        "{}.{}: {ns} nanoseconds is no whole microsecond",
                                        table.name, c.name
                                    )))
                                }
                                v => v.map(|ns| ns / 1000),
                            });
                        }
                    }
                    // An interval as the snail writes it out, its own 16 bytes.
                    (
                        Values::Interval(out),
                        ColumnType::Interval { .. },
                        DataType::FixedSizeBinary(16),
                    ) => {
                        let a = a.as_any().downcast_ref::<FixedSizeBinaryArray>().unwrap();
                        out.extend(
                            a.iter()
                                .map(|v| v.and_then(crate::times::Interval::from_bytes)),
                        );
                    }
                    (Values::Interval(out), ColumnType::Interval { .. }, DataType::Struct(_)) => {
                        let s = a
                            .as_any()
                            .downcast_ref::<arrow::array::StructArray>()
                            .unwrap();
                        let field = |name: &str| s.column_by_name(name).ok_or_else(wrong);
                        let (m, d, u) = (field("months")?, field("days")?, field("micros")?);
                        let (m, d, u) = match (
                            m.as_any().downcast_ref::<Int32Array>(),
                            d.as_any().downcast_ref::<Int32Array>(),
                            u.as_any().downcast_ref::<Int64Array>(),
                        ) {
                            (Some(m), Some(d), Some(u)) => (m, d, u),
                            _ => return Err(wrong()),
                        };
                        out.extend((0..s.len()).map(|r| {
                            (!s.is_null(r)).then(|| crate::times::Interval {
                                months: m.value(r),
                                days: d.value(r),
                                micros: u.value(r),
                            })
                        }));
                    }
                    (Values::Float4(out), ColumnType::Float4, DataType::Float32) => {
                        let a = a
                            .as_any()
                            .downcast_ref::<arrow::array::Float32Array>()
                            .unwrap();
                        out.extend(a.iter().map(|v| v.map(f32::to_bits)));
                    }
                    (Values::Float8(out), ColumnType::Float8, DataType::Float64) => {
                        let a = a
                            .as_any()
                            .downcast_ref::<arrow::array::Float64Array>()
                            .unwrap();
                        out.extend(a.iter().map(|v| v.map(f64::to_bits)));
                    }
                    (Values::Int(out), ColumnType::Boolean, DataType::Boolean) => {
                        let a = a.as_any().downcast_ref::<BooleanArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(i64::from)));
                    }
                    (Values::Int(out), ColumnType::Int2, DataType::Int16) => {
                        let a = a.as_any().downcast_ref::<Int16Array>().unwrap();
                        out.extend(a.iter().map(|v| v.map(i64::from)));
                    }
                    (Values::Int(out), ColumnType::Int4, DataType::Int32) => {
                        let a = a.as_any().downcast_ref::<Int32Array>().unwrap();
                        out.extend(a.iter().map(|v| v.map(i64::from)));
                    }
                    (Values::Int(out), ColumnType::Int8, DataType::Int64) => {
                        let a = a.as_any().downcast_ref::<Int64Array>().unwrap();
                        out.extend(a.iter());
                    }
                    (
                        Values::Int(out),
                        ColumnType::Timestamptz | ColumnType::TimestamptzRounded(_),
                        DataType::Timestamp(TimeUnit::Microsecond, _),
                    ) => {
                        let a = a
                            .as_any()
                            .downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap();
                        out.extend(a.iter());
                    }
                    (Values::Text(out), ty, dt)
                        if matches!(ty, ColumnType::Bpchar(_) | ColumnType::Name)
                            && matches!(
                                dt,
                                DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View
                            ) =>
                    {
                        let a = arrow::compute::cast(a, &DataType::Utf8)?;
                        let a = a.as_any().downcast_ref::<StringArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(str::to_string)));
                    }
                    // A value of a type held as its text, read as its input reads it; a node tree
                    // as it is.
                    (
                        Values::Text(out),
                        ty @ (ColumnType::Textual(_) | ColumnType::Internal(_)),
                        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View,
                    ) => {
                        let a = arrow::compute::cast(a, &DataType::Utf8)?;
                        let a = a.as_any().downcast_ref::<StringArray>().unwrap();
                        for v in a.iter() {
                            out.push(match (v, ty) {
                                (None, _) => None,
                                (Some(s), ColumnType::Textual(t)) => {
                                    Some(crate::textual::text_in(t, s)?)
                                }
                                (Some(s), ColumnType::Internal(t)) if t.copied() => {
                                    Some(s.to_string())
                                }
                                (Some(_), ColumnType::Internal(t)) => return Err(t.refusal()),
                                _ => unreachable!("a type held as its text"),
                            });
                        }
                    }
                    // An aclitem, its three fields made into the text the snail holds.
                    (
                        Values::Text(out),
                        ColumnType::Textual(crate::textual::Textual::Aclitem),
                        DataType::Struct(_),
                    ) => {
                        for r in 0..a.len() {
                            out.push(if a.is_null(r) {
                                None
                            } else {
                                Some(crate::strings::aclitem_at(a.as_ref(), r).ok_or_else(wrong)?)
                            });
                        }
                    }
                    // A geometric value, its float8s each a field of the group.
                    (Values::Utf8(out), ColumnType::Geometric(_), DataType::Struct(fields))
                        if fields.len() == c.ty.leaf_count() =>
                    {
                        let s = a
                            .as_any()
                            .downcast_ref::<arrow::array::StructArray>()
                            .ok_or_else(wrong)?;
                        let leaves: Vec<&arrow::array::Float64Array> = s
                            .columns()
                            .iter()
                            .map(|l| l.as_any().downcast_ref::<arrow::array::Float64Array>())
                            .collect::<Option<_>>()
                            .ok_or_else(wrong)?;
                        for r in 0..s.len() {
                            out.push((!s.is_null(r)).then(|| {
                                ByteArray::from(
                                    leaves
                                        .iter()
                                        .flat_map(|l| l.value(r).to_bits().to_be_bytes())
                                        .collect::<Vec<u8>>(),
                                )
                            }));
                        }
                    }
                    // A statistics type's bytes, as they are; gtsvector's and the BRIN summaries'
                    // NULLs alone.
                    (
                        Values::Utf8(out),
                        ColumnType::Internal(t),
                        DataType::Binary | DataType::LargeBinary | DataType::BinaryView,
                    ) => {
                        let a = arrow::compute::cast(a, &DataType::Binary)?;
                        let a = a.as_any().downcast_ref::<BinaryArray>().unwrap();
                        for v in a.iter() {
                            out.push(match v {
                                None => None,
                                Some(_) if !t.copied() => return Err(t.refusal()),
                                Some(b) => Some(ByteArray::from(b.to_vec())),
                            });
                        }
                    }
                    (Values::Utf8(out), ty, dt)
                        if ty.is_bits() && *dt == crate::strings::string_arrow(ty) =>
                    {
                        out.extend((0..a.len()).map(|r| {
                            crate::strings::bits_key_at(ty, a.as_ref(), r).map(ByteArray::from)
                        }));
                    }
                    (Values::Text(out), ColumnType::Text(_), DataType::Utf8) => {
                        let a = a.as_any().downcast_ref::<StringArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(str::to_string)));
                    }
                    (Values::Text(out), ColumnType::Text(_), DataType::LargeUtf8) => {
                        let a = a.as_any().downcast_ref::<LargeStringArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(str::to_string)));
                    }
                    (Values::Text(out), ColumnType::Text(_), DataType::Utf8View) => {
                        let a = a.as_any().downcast_ref::<StringViewArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(str::to_string)));
                    }
                    (Values::Utf8(out), ColumnType::Bytes, DataType::Binary) => {
                        let a = a.as_any().downcast_ref::<BinaryArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(|b| ByteArray::from(b.to_vec()))));
                    }
                    (Values::Utf8(out), ColumnType::Bytes, DataType::LargeBinary) => {
                        let a = a.as_any().downcast_ref::<LargeBinaryArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(|b| ByteArray::from(b.to_vec()))));
                    }
                    (Values::Utf8(out), ColumnType::Bytes, DataType::BinaryView) => {
                        let a = a.as_any().downcast_ref::<BinaryViewArray>().unwrap();
                        out.extend(a.iter().map(|v| v.map(|b| ByteArray::from(b.to_vec()))));
                    }
                    // A numeric(p, s): the group of its decimal and whether it is NaN, as `out`
                    // writes it, or a decimal alone, as another writer gives one.
                    (
                        Values::Int(_) | Values::Wide(_),
                        ColumnType::Numeric { precision, scale },
                        dt,
                    ) if crate::numeric::decimal_type(dt).is_some() => {
                        let limit = 10i128.pow(precision as u32);
                        for r in 0..a.len() {
                            let units = match crate::numeric::decimal_at(a.as_ref(), r) {
                                None => None,
                                Some((None, _, _)) => Some(crate::numeric::nan_units(precision)),
                                // A value at a scale finer than the column's is refused unless
                                // its extra digits are zeros.
                                Some((Some(x), _, s)) => Some(
                                    crate::numeric::Numeric::of_units(x, s as u32)
                                        .units_at(u32::from(scale))
                                        .filter(|u| u.abs() < limit)
                                        .ok_or_else(|| {
                                            Error::new(format!(
                                                "{}.{}: a value is no {}",
                                                table.name,
                                                c.name,
                                                c.ty.name()
                                            ))
                                        })?,
                                ),
                            };
                            match &mut out {
                                Values::Int(o) => o.push(units.map(|u| u as i64)),
                                Values::Wide(o) => o.push(units),
                                _ => unreachable!("a numeric's units"),
                            }
                        }
                    }
                    // A numeric held as its text, each value read as numeric_in reads it and held
                    // to the column's typmod.
                    (Values::Text(out), ColumnType::NumericText(_), DataType::Utf8) => {
                        let a = a.as_any().downcast_ref::<StringArray>().unwrap();
                        for v in a.iter() {
                            out.push(match v {
                                None => None,
                                Some(s) => {
                                    match crate::read::fill::numeric_datum(
                                        crate::numeric::Numeric::parse(s)?,
                                        c.ty,
                                    )? {
                                        crate::read::statement::Datum::Text(t) => Some(t),
                                        _ => unreachable!("a numeric held as its text"),
                                    }
                                }
                            });
                        }
                    }
                    _ => return Err(wrong()),
                }
            }
            columns.push(out);
        }
        Ok(Rows { columns })
    }
}

/// A row's value of a column, as a statement holds it.
fn row_value(ty: ColumnType, v: &Values, i: usize) -> crate::read::statement::Datum {
    use crate::read::statement::Datum;
    if v.is_null(i) {
        return Datum::Null;
    }
    match (ty, v) {
        (_, Values::Int(x)) => x[i].map_or(Datum::Null, Datum::Int),
        (_, Values::Wide(x)) => x[i].map_or(Datum::Null, Datum::Wide),
        (ty, _) if ty.held_as_bytes() => v
            .text_value(i)
            .map_or(Datum::Null, |b| Datum::Bytes(b.data().to_vec())),
        _ => v
            .text(i)
            .map_or(Datum::Null, |s| Datum::Text(s.to_string())),
    }
}

/// What a write laid out, order by order.
#[derive(Debug)]
#[non_exhaustive]
pub struct Written {
    pub table: String,
    pub order: String,
    pub rows: usize,
    pub blocks: usize,
    /// The directory's pages, its top among them, and its levels.
    pub index_pages: u64,
    pub levels: u32,
    pub map_pages: u64,
    pub columns: Vec<ColumnWritten>,
}

#[derive(Debug)]
#[non_exhaustive]
pub struct ColumnWritten {
    pub name: String,
    pub k: usize,
    pub dictionary_blocks: usize,
    pub data_pages: usize,
    pub disk_pages: usize,
    /// The blocks whose pages of this column came in as they lay.
    pub moved_blocks: usize,
    /// The bytes of the text present, for a text column.
    pub bytes: Option<u64>,
}

/// A table's rows as a Parquet file holds them: each row group's first row and rows, and for each
/// row group and each of the table's leaves, in its columns' order, the chunk's pages where they can
/// come in as they lie.
pub struct Source {
    pub groups: Vec<(usize, usize)>,
    pub chunks: Vec<Vec<Option<Encoded>>>,
}

/// The row groups of a Parquet file holding a table's rows, and the pages of each column chunk that
/// can come in as they lie: uncompressed version 1 pages, plain or on a dictionary, each leaf of its
/// column's own type, found by its path.
pub fn source(path: &Path, table: &Table) -> Result<Source> {
    use parquet::file::metadata::{ParquetMetaDataOptions, ParquetMetaDataReader};
    let file =
        std::fs::File::open(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))?;
    let meta = ParquetMetaDataReader::new()
        .with_page_index_policy(parquet::file::metadata::PageIndexPolicy::Optional)
        .with_metadata_options(Some(
            ParquetMetaDataOptions::new().with_encoding_stats_as_mask(false),
        ))
        .parse_and_finish(&file)?;
    let schema = meta.file_metadata().schema_descr();
    let leaves = table.leaves();
    let at: Vec<Option<usize>> = leaves
        .iter()
        .map(|(c, leaf)| {
            let path = leaf.path(&table.columns[*c].name);
            (0..schema.num_columns()).find(|&i| schema.column(i).path().parts() == path)
        })
        .collect();
    let ours = leaves
        .iter()
        .map(|(c, leaf)| {
            let col = &table.columns[*c];
            leaf_descriptor(&col.name, leaf, !col.not_null)
        })
        .collect::<Result<Vec<_>>>()?;
    let mut groups = Vec::new();
    let mut chunks = Vec::new();
    let mut first = 0;
    for (g, rg) in meta.row_groups().iter().enumerate() {
        let n = rg.num_rows() as usize;
        groups.push((first, n));
        first += n;
        let mut row = Vec::with_capacity(leaves.len());
        for (l, desc) in ours.iter().enumerate() {
            let offsets = meta
                .offset_index()
                .and_then(|o| o.get(g))
                .and_then(|o| at[l].and_then(|i| o.get(i)));
            row.push(match (at[l], offsets) {
                (Some(i), Some(o)) => movable(&file, rg.column(i), o, desc, n)?,
                _ => None,
            });
        }
        chunks.push(row);
    }
    Ok(Source { groups, chunks })
}

/// A table's rows as several Parquet files hold them, one after another: their row groups in turn.
pub fn sources(paths: &[std::path::PathBuf], table: &Table) -> Result<Source> {
    let mut out = Source {
        groups: Vec::new(),
        chunks: Vec::new(),
    };
    let mut before = 0;
    for p in paths {
        let s = source(p, table)?;
        for &(first, n) in &s.groups {
            out.groups.push((before + first, n));
        }
        before += s.groups.iter().map(|g| g.1).sum::<usize>();
        out.chunks.extend(s.chunks);
    }
    Ok(out)
}

/// A table's Parquet files: the file named, or the files of the directory named, in the order of
/// their numbers (`0.parquet`, `1.parquet` and on), as `out` writes a table past the span.
pub fn parquet_files(path: &Path) -> Result<Vec<std::path::PathBuf>> {
    if !path.is_dir() {
        return Ok(vec![path.to_path_buf()]);
    }
    let mut files: Vec<(u64, std::path::PathBuf)> = Vec::new();
    for e in std::fs::read_dir(path).map_err(|e| Error::new(format!("{}: {e}", path.display())))? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "parquet") {
            let n = p
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| {
                    Error::new(format!("{}: a table's files are numbered", p.display()))
                })?;
            files.push((n, p));
        }
    }
    files.sort();
    Ok(files.into_iter().map(|(_, p)| p).collect())
}

/// What a table's Parquet files name of the snail they came out of, as `out` names it: the
/// PostgreSQL release its OIDs came from, and the lc_monetary its money was written under.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Named {
    pub release: Option<u8>,
    pub lc_monetary: Option<String>,
}

/// What a table's files name, each by its key-value metadata: refused where two of them name
/// different ones, and where a release is no number.
pub fn named(table: &str, paths: &[std::path::PathBuf]) -> Result<Named> {
    use crate::read::{MONETARY_KEY, RELEASE_KEY};
    let mut out = Named::default();
    for p in paths {
        let file =
            std::fs::File::open(p).map_err(|e| Error::new(format!("{}: {e}", p.display())))?;
        let meta = parquet::file::metadata::ParquetMetaDataReader::new().parse_and_finish(&file)?;
        let kv = meta.file_metadata().key_value_metadata();
        let value = |key: &str| {
            kv.and_then(|kv| kv.iter().find(|k| k.key == key))
                .and_then(|k| k.value.clone())
        };
        if let Some(r) = value(RELEASE_KEY) {
            let r: u8 = r.parse().map_err(|_| {
                Error::new(format!(
                    "{table}: {} names the release {r} under {RELEASE_KEY}, which is no release",
                    p.display()
                ))
            })?;
            match out.release {
                Some(was) if was != r => {
                    return Err(Error::new(format!(
                        "{table}: its files name the releases {was} and {r} under {RELEASE_KEY}"
                    )))
                }
                _ => out.release = Some(r),
            }
        }
        if let Some(l) = value(MONETARY_KEY) {
            match &out.lc_monetary {
                Some(was) if !crate::monetary::Monetary::names_same(was, &l) => {
                    return Err(Error::new(format!(
                        "{table}: its files name the lc_monetary {was} and {l} under \
                         {MONETARY_KEY}"
                    )))
                }
                _ => out.lc_monetary = Some(l),
            }
        }
    }
    Ok(out)
}

/// The release and the lc_monetary a write from Parquet files takes: what its tables' files name,
/// alike in each; else what the flags give, `--release` and `--lc-monetary`; else 19 and C. A flag
/// naming another than the files name is refused by name.
pub fn settled(
    named: &[(String, Named)],
    release: Option<u8>,
    lc_monetary: Option<&str>,
) -> Result<(u8, Option<String>)> {
    use crate::read::{MONETARY_KEY, RELEASE_KEY};
    let mut file_release: Option<(&str, u8)> = None;
    let mut file_monetary: Option<(&str, &str)> = None;
    for (table, n) in named {
        if let Some(r) = n.release {
            match file_release {
                Some((other, was)) if was != r => {
                    return Err(Error::new(format!(
                        "{table}'s file names the release {r} under {RELEASE_KEY}, and {other}'s \
                         {was}: a snail's OIDs come from one release"
                    )))
                }
                Some(_) => {}
                None => file_release = Some((table, r)),
            }
        }
        if let Some(l) = n.lc_monetary.as_deref() {
            match file_monetary {
                Some((other, was)) if !crate::monetary::Monetary::names_same(was, l) => {
                    return Err(Error::new(format!(
                        "{table}'s file names the lc_monetary {l} under {MONETARY_KEY}, and \
                         {other}'s {was}: the snail answers under one"
                    )))
                }
                Some(_) => {}
                None => file_monetary = Some((table, l)),
            }
        }
    }
    let release = match (file_release, release) {
        (Some((table, was)), Some(r)) if r != was => {
            return Err(Error::new(format!(
                "--release {r}, and {table}'s file names the release {was} under {RELEASE_KEY}: \
                 write takes the release its files name"
            )))
        }
        (Some((_, was)), _) => was,
        (None, Some(r)) => r,
        (None, None) => crate::regs::LATEST,
    };
    let lc_monetary = match (file_monetary, lc_monetary) {
        (Some((table, was)), Some(l)) if !crate::monetary::Monetary::names_same(was, l) => {
            return Err(Error::new(format!(
                "--lc-monetary {l}, and {table}'s file names the lc_monetary {was} under \
                 {MONETARY_KEY}: write takes the lc_monetary its files name"
            )))
        }
        (Some((_, was)), _) => Some(was.to_string()),
        (None, l) => l.map(str::to_string),
    };
    Ok((release, lc_monetary))
}

/// A new snail from Parquet files, as the command's `write` takes them: each table's rows from its
/// file, or the files of a directory as `out` writes a table past the span, a row group cut as the
/// snail cuts it coming in by its pages; under the release and the lc_monetary `settled` gives,
/// which every column holding money is read under.
pub fn write_parquet(
    path: &Path,
    catalog: &Catalog,
    given: &[(String, std::path::PathBuf)],
    span: u64,
    release: Option<u8>,
    lc_monetary: Option<&str>,
) -> Result<Vec<Written>> {
    write_parquet_in(path, catalog, given, span, release, lc_monetary, None)
}

/// A new snail as `write_parquet` writes it, in format 7 or 8 where `format` names one, as
/// warren-snail 0.1.0 or 0.1.1 made it: for the checks that hold this build to them.
#[doc(hidden)]
pub fn write_parquet_in(
    path: &Path,
    catalog: &Catalog,
    given: &[(String, std::path::PathBuf)],
    span: u64,
    release: Option<u8>,
    lc_monetary: Option<&str>,
    format: Option<u32>,
) -> Result<Vec<Written>> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let mut tables = Vec::new();
    let mut sourced = Vec::new();
    let mut names = Vec::new();
    for (name, p) in given {
        let t = catalog
            .table(name)
            .ok_or_else(|| Error::new(format!("{name} is not in the DDL")))?;
        let files = parquet_files(p)?;
        let mut batches = Vec::new();
        for f in &files {
            let file =
                std::fs::File::open(f).map_err(|e| Error::new(format!("{}: {e}", f.display())))?;
            batches.extend(
                ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        tables.push((name.clone(), Rows::from_batches(t, &batches)?));
        sourced.push((name.clone(), sources(&files, t)?));
        names.push((name.clone(), named(name, &files)?));
    }
    let (release, lc_monetary) = settled(&names, release, lc_monetary)?;
    let mut carried = Carried::default();
    if let Some(l) = lc_monetary {
        for t in &catalog.tables {
            for c in t.columns.iter().filter(|c| c.ty.holds_money()) {
                carried
                    .monetary
                    .push((t.name.clone(), c.name.clone(), l.clone()));
            }
        }
    }
    if let Some(f) = format.filter(|&f| f != FORMAT_SEVEN && f != FORMAT_EIGHT) {
        return Err(Error::new(format!(
            "--format takes {FORMAT_SEVEN} or {FORMAT_EIGHT}, a format before this build's, not {f}"
        )));
    }
    write_made(
        path,
        catalog,
        &tables,
        &sourced,
        span,
        format,
        release,
        Some(&carried),
    )
}

/// A column chunk's pages, where they can come in as they lie: the chunk of a leaf the snail
/// describes as `ours`.
fn movable(
    file: &std::fs::File,
    chunk: &parquet::file::metadata::ColumnChunkMetaData,
    offsets: &parquet::file::page_index::offset_index::OffsetIndexMetaData,
    ours: &parquet::schema::types::ColumnDescPtr,
    rows: usize,
) -> Result<Option<Encoded>> {
    use parquet::basic::{Compression, Encoding, PageType};
    use std::os::unix::fs::FileExt;
    let pages_held = chunk.page_encoding_stats().is_some_and(|s| {
        s.iter().all(|p| {
            matches!(
                (p.page_type, p.encoding),
                (
                    PageType::DICTIONARY_PAGE,
                    Encoding::PLAIN | Encoding::PLAIN_DICTIONARY
                ) | (
                    PageType::DATA_PAGE,
                    Encoding::PLAIN | Encoding::PLAIN_DICTIONARY | Encoding::RLE_DICTIONARY
                )
            )
        })
    });
    // A chunk whose dictionary ran out, its later pages plain, is neither encoding the counts choose.
    let mixed = chunk.page_encoding_stats().is_some_and(|s| {
        s.iter().any(|p| p.page_type == PageType::DICTIONARY_PAGE)
            && s.iter()
                .any(|p| p.page_type == PageType::DATA_PAGE && p.encoding == Encoding::PLAIN)
    });
    if chunk.compression() != Compression::UNCOMPRESSED
        || chunk.column_descr() != ours.as_ref()
        || !pages_held
        || mixed
    {
        return Ok(None);
    }
    let read = |at: i64, len: usize| -> Result<Vec<u8>> {
        let mut buf = vec![0u8; len];
        file.read_exact_at(&mut buf, at as u64)?;
        Ok(buf)
    };
    let dictionary = chunk
        .dictionary_page_offset()
        .map(|d| read(d, (chunk.data_page_offset() - d) as usize))
        .transpose()?;
    let locations = offsets.page_locations();
    let mut pages = Vec::with_capacity(locations.len());
    for (i, l) in locations.iter().enumerate() {
        let first = l.first_row_index as usize;
        let next = locations
            .get(i + 1)
            .map_or(rows, |n| n.first_row_index as usize);
        pages.push(DataPage {
            bytes: read(l.offset, l.compressed_page_size as usize)?,
            first_row: first,
            rows: next - first,
        });
    }
    Ok(Some(Encoded { dictionary, pages }))
}

pub fn write(path: &Path, catalog: &Catalog, tables: &[(String, Rows)]) -> Result<Vec<Written>> {
    write_from(path, catalog, tables, &[])
}

/// A new snail from each table's rows. Where a table's rows come with their Parquet file's pages, a
/// row group that is one block of an order's cut, each of its pages within the column's counting
/// unit, comes in by its pages as they lie; every other block is encoded by the counts.
pub fn write_from(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
) -> Result<Vec<Written>> {
    write_spanned(path, catalog, tables, sources, FILE_PAGES)
}

/// A new snail as `write_from` writes it, its pages in files of `span` pages each.
pub fn write_spanned(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    span: u64,
) -> Result<Vec<Written>> {
    write_made(
        path,
        catalog,
        tables,
        sources,
        span,
        None,
        crate::regs::LATEST,
        None,
    )
}

/// A new snail as `write_spanned` writes it, its OIDs from PostgreSQL `release`, whose catalog
/// names its reg* values: 14 to 19.
pub fn write_released(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    span: u64,
    release: u8,
) -> Result<Vec<Written>> {
    write_made(path, catalog, tables, sources, span, None, release, None)
}

/// What a snail written afresh from another carries over of it beyond its rows: where its
/// sequences stand, the `lc_monetary` each money column was written under, and the source it is a
/// replica of.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct Carried {
    pub sequences: Vec<crate::file::SequenceState>,
    pub monetary: Vec<(String, String, String)>,
    pub replica: Option<String>,
}

/// A new snail as `write_released` writes it, carrying over what `carried` holds of the snail its
/// rows came from.
pub fn write_carried(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    span: u64,
    release: u8,
    carried: &Carried,
) -> Result<Vec<Written>> {
    write_made(
        path,
        catalog,
        tables,
        sources,
        span,
        None,
        release,
        Some(carried),
    )
}

/// A new snail as warren-snail 0.1.0 made it, in format 7, each page map in pages of its own: for the
/// tests that read and write such a snail.
#[doc(hidden)]
pub fn write_format_seven(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    span: u64,
) -> Result<Vec<Written>> {
    write_made(
        path,
        catalog,
        tables,
        &[],
        span,
        Some(FORMAT_SEVEN),
        crate::regs::LATEST,
        None,
    )
}

/// A new snail as warren-snail 0.1.1 made it, in format 8: for the tests that read and write such a
/// snail.
#[doc(hidden)]
pub fn write_format_eight(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    span: u64,
) -> Result<Vec<Written>> {
    write_made(
        path,
        catalog,
        tables,
        &[],
        span,
        Some(FORMAT_EIGHT),
        crate::regs::LATEST,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
fn write_made(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    span: u64,
    format: Option<u32>,
    release: u8,
    carried: Option<&Carried>,
) -> Result<Vec<Written>> {
    for (name, _) in tables {
        if catalog.table(name).is_none() {
            return Err(Error::new(format!("{name} is not in the DDL")));
        }
    }
    let mut named = catalog.clone();
    named.with_release(release)?;
    let monetary = crate::file::monetary_of(&named, carried.map_or(&[], |c| &c.monetary));
    named.with_monetary(&monetary)?;
    let mut w = Writer::create_spanned(path, span)?;
    if let Some(f) = format {
        w.set_format(f);
        crate::read::old_names(&mut named, f);
    }
    let written = write_tables(w, &named, tables, sources, carried);
    if written.is_err() {
        // A write refused leaves no file of its own behind.
        let _ = std::fs::remove_file(path);
        for n in 1.. {
            let mut p = path.as_os_str().to_owned();
            p.push(format!(".{n}"));
            if std::fs::remove_file(&p).is_err() {
                break;
            }
        }
    }
    written
}

/// Every table of the catalog written into a new snail, and the root committed. A table given no
/// rows is written empty.
fn write_tables(
    mut w: Writer,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    carried: Option<&Carried>,
) -> Result<Vec<Written>> {
    let mut root = Root {
        generation: 1,
        release: catalog.release,
        monetary: crate::file::monetary_of(catalog, carried.map_or(&[], |c| &c.monetary)),
        replica: carried.and_then(|c| c.replica.clone()),
        ..Default::default()
    };
    crate::file::holds_catalog(w.format(), catalog)?;
    crate::read::foreign::rows_held(catalog, tables)?;
    let mut out = Vec::new();
    for table in &catalog.tables {
        crate::file::holds(w.format(), table)?;
        let Some(rows) = tables
            .iter()
            .find(|(n, _)| *n == table.name)
            .map(|(_, r)| r)
        else {
            for order in &table.orders {
                let format = w.format();
                let (extent, written) = empty_order(&mut w, table, order, format)?;
                root.orders
                    .push((table.name.clone(), order.name.clone(), extent));
                out.push(written);
            }
            continue;
        };
        for (ci, (c, values)) in table.columns.iter().zip(&rows.columns).enumerate() {
            if values.len() != rows.len() {
                return Err(Error::new(format!(
                    "{}: its columns differ in length",
                    table.name
                )));
            }
            if let (ColumnType::Numeric { precision, .. }, Some(v)) = (c.ty, values.ints()) {
                let nan = crate::numeric::nan_units(precision) as i64;
                if v.contains(&Some(nan)) {
                    crate::file::holds_nan(w.format(), table, ci)?;
                }
            }
            if let ColumnType::Text(Some(n)) = c.ty {
                let longest = values.longest();
                if longest > n as usize {
                    return Err(Error::new(format!(
                        "{}.{} is varchar({n}) and a row holds {longest} characters",
                        table.name, c.name
                    )));
                }
            }
        }
        // Each row in turn meets its columns' domains, then the table's NOT NULLs and CHECKs, as
        // PostgreSQL's COPY checks them.
        let nulls = table
            .columns
            .iter()
            .zip(&rows.columns)
            .any(|(c, v)| c.not_null && v.nulls() > 0);
        if nulls
            || table.checks.iter().any(|k| k.enforced)
            || table.columns.iter().any(|c| c.domain.is_some())
        {
            for i in 0..rows.len() {
                let row: crate::read::statement::Row = table
                    .columns
                    .iter()
                    .zip(&rows.columns)
                    .map(|(c, v)| row_value(c.ty, v, i))
                    .collect();
                crate::read::fill::check_domains(&catalog.domains, table, &row)?;
                crate::read::fill::check(table, &row)?;
            }
        }
        // An EXCLUDE, and a key WITHOUT OVERLAPS, are checked where a statement writes its rows.
        if let Some(o) = table.orders.iter().find(|o| o.exclusion.is_some()) {
            if !rows.is_empty() {
                let what = if o.without_overlaps() {
                    "key WITHOUT OVERLAPS"
                } else {
                    "EXCLUDE"
                };
                return Err(Error::new(format!(
                    "{}: write does not hold its rows to the {what} {}: give them by INSERT",
                    table.name, o.name
                )));
            }
        }
        let source = sources
            .iter()
            .find(|(n, _)| *n == table.name)
            .map(|(_, s)| s);
        for (c, col) in table.columns.iter().enumerate() {
            if col.ty.is_timestamptz()
                && rows.columns[c]
                    .ints()
                    .is_some_and(|v| v.iter().any(|x| matches!(x, Some(i64::MAX | i64::MIN))))
            {
                crate::file::instant_ends(w.format(), table, c)?;
            }
        }
        for order in &table.orders {
            // A partial strand holds the rows its predicate admits, encoded afresh.
            let admitted = match crate::read::partial::Predicate::of(catalog, table, order)? {
                Some(p) => {
                    let kept: Vec<usize> = (0..rows.len())
                        .filter(|&r| p.admits_at(table, rows, r))
                        .collect();
                    Some(Rows {
                        columns: rows.columns.iter().map(|v| v.gather(&kept)).collect(),
                    })
                }
                None => None,
            };
            let (rows, source) = match &admitted {
                Some(r) => (r, None),
                None => (rows, source),
            };
            if order
                .levels
                .iter()
                .any(|l| rows.columns[l.column()].nulls() > 0)
            {
                crate::file::keys_null(w.format(), table, order)?;
            }
            let packed = w.format() != FORMAT_SEVEN;
            let format = w.format();
            let (extent, written) =
                write_order(&mut w, table, order, rows, source, packed, format)?;
            root.orders
                .push((table.name.clone(), order.name.clone(), extent));
            out.push(written);
        }
    }
    root.catalog = w.place(catalog.ddl.as_bytes())?;
    if let Some(c) = carried.filter(|c| !c.sequences.is_empty()) {
        root.sequences = w.place(&crate::file::encode_sequences(&c.sequences))?;
    }
    // The pages passed over at a file's end, where a run would have crossed into the next, are free.
    let passed: Vec<Run> = w
        .left_free()
        .into_iter()
        .map(|(start, pages)| Run {
            start,
            pages,
            tag: 0,
        })
        .collect();
    if !passed.is_empty() {
        root.free = w.place_end(&encode_free(&passed, &[], w.format()))?;
    }
    w.commit(&mut root)?;
    Ok(out)
}

/// Rows of a table sorted on one of its orders, and cut into blocks.
pub(crate) struct Laid {
    /// The rows in the order's order, by their place among the rows given.
    pub perm: Vec<usize>,
    pub keys: Keys,
    pub text_sublevels: Vec<u32>,
    pub widths: Widths,
    pub blocks: Vec<Block>,
}

/// Sorts rows on an order and cuts them into blocks. A text level is cut on the sublevels `given`, or,
/// where none are given, on its longest value's bytes and one more.
pub(crate) fn laid_out(
    table: &Table,
    order: &Order,
    rows: &Rows,
    given: Option<&[u32]>,
    format: u32,
) -> Result<Laid> {
    let n = rows.len();
    let stride = order.levels.len();
    let mut unsorted: Vec<KeyValue> = Vec::with_capacity(n * stride);
    // For each level, the sublevels a text level is cut on: its longest value's bytes and one more;
    // at least one, as an empty order's, where it holds only NULLs.
    let mut text_sublevels = level_kinds(table, order, format);
    let fixed = given.is_some();
    if let Some(g) = given {
        text_sublevels = g.to_vec();
    }
    let collators = order
        .levels
        .iter()
        .map(|l| table.columns[l.column()].orderer())
        .collect::<Result<Vec<_>>>()?;
    for r in 0..n {
        for (li, &l) in order.levels.iter().enumerate() {
            let column = &rows.columns[l.column()];
            if column.is_null(r) {
                unsorted.push(null_key(order, li));
                continue;
            }
            let v = match column {
                // A composed value by its key, written as text that sorts as the key does.
                Values::Composed(x) => {
                    let ty = table.columns[l.column()].ty;
                    let key = x[r].as_ref().unwrap().hex.as_bytes().to_vec();
                    if !fixed {
                        text_sublevels[li] = crate::places::rungs_for(ty, &key, text_sublevels[li]);
                    }
                    unsorted.push(crate::places::byte_key(ty, &key));
                    continue;
                }
                Values::Text(_) | Values::Utf8(_) | Values::Dict { .. } => {
                    let ty = table.columns[l.column()].ty;
                    let held = column.text_value(r).unwrap();
                    let key = crate::strings::column_key(ty, collators[li], held.data());
                    if !fixed {
                        text_sublevels[li] = crate::places::rungs_for(ty, &key, text_sublevels[li]);
                    }
                    unsorted.push(crate::places::column_byte_key(
                        &table.columns[l.column()],
                        &key,
                    ));
                    continue;
                }
                Values::Wide(v) => {
                    unsorted.push(KeyValue::Wide(v[r].unwrap()));
                    continue;
                }
                Values::Interval(_) => {
                    unsorted.push(KeyValue::Wide(column.wide(r).unwrap()));
                    continue;
                }
                Values::Float4(_) | Values::Float8(_) => column.int(r).unwrap(),
                Values::Int(v) => v[r].unwrap(),
                Values::Nested(_) => unreachable!("a level reads its column whole"),
            };
            unsorted.push(level_key(order, li, v));
        }
    }
    let row = |r: usize| &unsorted[r * stride..(r + 1) * stride];
    let mut perm: Vec<usize> = (0..n).collect();
    perm.sort_by(|&a, &b| row(a).cmp(row(b)));
    for (li, c) in collators.iter().enumerate() {
        if let Some(c) = c {
            agreed(table, order.levels[li].column(), c, rows)?;
        }
    }
    if order.unique {
        // Keys holding a NULL are each their own, unless the order says they are not distinct.
        let shared = |a: &[KeyValue], b: &[KeyValue]| {
            a == b && !(order.nulls_distinct && a.iter().any(KeyValue::is_null))
        };
        if let Some(p) = perm.windows(2).find(|p| shared(row(p[0]), row(p[1]))) {
            return Err(Error::new(format!(
                "{}: two rows share the value {} of {}, which is unique",
                table.name,
                crate::places::shown_as(
                    row(p[0]),
                    &order
                        .levels
                        .iter()
                        .map(|l| table.columns[l.column()].ty)
                        .collect::<Vec<_>>()
                ),
                order.name
            )));
        }
    }
    let keys = Keys {
        stride,
        data: perm.iter().flat_map(|&r| row(r).iter().cloned()).collect(),
    };
    let sublevels = Sublevels::new(order, table, &text_sublevels);
    let mut widths = Widths(Vec::new());
    for &c in &order.columns {
        let leaves = table.column_leaves(c);
        for (leaf, values) in leaves.iter().zip(leaf_values(table, c, &rows.columns[c])) {
            widths.0.push(width(leaf, &values, &perm));
        }
    }
    let blocks = cut(&keys, &sublevels, &widths);
    Ok(Laid {
        perm,
        keys,
        text_sublevels,
        widths,
        blocks,
    })
}

/// A column's values as each of the leaves its table holds it as holds them: the column's own where
/// it is one leaf.
pub(crate) fn leaf_values<'a>(
    table: &Table,
    column: usize,
    values: &'a Values,
) -> Vec<std::borrow::Cow<'a, Values>> {
    if !table.is_group(column) {
        vec![std::borrow::Cow::Borrowed(values)]
    } else {
        values
            .clone()
            .into_leaves(table.columns[column].ty)
            .into_iter()
            .map(std::borrow::Cow::Owned)
            .collect()
    }
}

/// What a leaf takes written plain, its rows in the order `perm` lays them.
fn width(leaf: &crate::catalog::Leaf, values: &Values, perm: &[usize]) -> Width {
    let ty = leaf.ty;
    let summed = |each: &dyn Fn(usize) -> u64| {
        let mut sum = Vec::with_capacity(perm.len() + 1);
        sum.push(0u64);
        for &r in perm {
            sum.push(sum.last().unwrap() + each(r));
        }
        sum
    };
    // An array's dimensions and lower bounds: the cut passes over them, their bits counted.
    if leaf.thin {
        return Width::Bits(summed(&|r| values.plain(r, ty) * 8));
    }
    if let Some(bits) = ty.thin_bits() {
        if let Values::Nested(n) = values {
            return Width::Bits(summed(&|r| {
                n.entries(r).filter(|&e| !n.values.is_null(e)).count() as u64 * bits
            }));
        }
        return Width::Bits(summed(&|r| if values.is_null(r) { 0 } else { bits }));
    }
    // A leaf of a composed column takes its entries' values, whatever its type's width; a row none
    // of whose entries holds a value holds none on the leaf, as a NULL holds none.
    let width = if matches!(values, Values::Nested(_)) {
        None
    } else {
        ty.width()
    };
    let null = |r: usize| match values {
        Values::Nested(n) => n.entries(r).all(|e| n.values.is_null(e)),
        _ => values.is_null(r),
    };
    match (width, (0..values.len()).any(null)) {
        (Some(wd), false) => Width::Fixed(wd),
        (None, false) => Width::Varying(summed(&|r| values.plain(r, ty))),
        (_, true) => Width::Nulls {
            bytes: summed(&|r| values.plain(r, ty)),
            nulls: summed(&|r| null(r) as u64),
        },
    }
}

/// What each level of an order holds before any row is laid on it: a text level cut on one
/// sublevel, a level keyed in 128 bits marked so, and a number 0; in a snail of format 9 a level keyed
/// by its bytes or its text marked to be cut a bit a rung, or on its structure.
pub(crate) fn level_kinds(table: &Table, order: &Order, format: u32) -> Vec<u32> {
    order
        .levels
        .iter()
        .map(|l| {
            let ty = table.columns[l.column()].ty;
            let bits = if format >= crate::file::FORMAT && !ty.is_bits() {
                crate::directory::BITS_LEVEL
            } else {
                0
            };
            if ty.is_wide() {
                WIDE_LEVEL
            } else if ty.keyed_by_bytes() || table.columns[l.column()].collated() {
                crate::directory::BYTES_LEVEL | bits | 1
            } else if ty.keyed_by_text() {
                bits | 1
            } else {
                0
            }
        })
        .collect()
}

/// Refuses, by name, a level's values that glibc's `strxfrm_l` keys in another order than its
/// `strcoll_l` compares them: each value against the next in its keys' order. ICU's sort keys are
/// its order, and pass.
fn agreed(
    table: &Table,
    column: usize,
    collator: &crate::collations::Collator,
    rows: &Rows,
) -> Result<()> {
    if collator.spec.provider != crate::collations::Provider::Libc {
        return Ok(());
    }
    let values = &rows.columns[column];
    let mut keyed: Vec<(Vec<u8>, Vec<u8>)> = (0..rows.len())
        .filter_map(|r| values.text_value(r))
        .map(|v| {
            let held = crate::strings::key_bytes(table.columns[column].ty, v.data()).into_owned();
            (collator.key(&held), held)
        })
        .collect();
    keyed.sort();
    keyed.dedup();
    for p in keyed.windows(2) {
        if collator.cmp(&p[0].1, &p[1].1) != p[0].0.cmp(&p[1].0) {
            return Err(disagreed(table, column, &p[0].1, &p[1].1));
        }
    }
    Ok(())
}

/// Refuses, by name, a key written beside the first key of the block after its own, where on the
/// first level they part a glibc collation's `strxfrm_l` keys them in another order than its
/// `strcoll_l` compares them: the neighbour no layout of its own block sees.
pub(crate) fn agreed_beside(
    table: &Table,
    order: &Order,
    key: &[KeyValue],
    next: &[KeyValue],
) -> Result<()> {
    let Some(li) = key.iter().zip(next).position(|(a, b)| a != b) else {
        return Ok(());
    };
    let column = order.levels[li].column();
    let Some(collator) = table.columns[column].orderer()? else {
        return Ok(());
    };
    if collator.spec.provider != crate::collations::Provider::Libc || collator.spec.collate_is_c() {
        return Ok(());
    }
    // A glibc level's key is strxfrm_l's bytes, a NUL, then the value's own.
    let held = |k: &KeyValue| match k {
        KeyValue::Bytes(b) => b
            .iter()
            .position(|&x| x == 0)
            .map(|at| b[at + 1..].to_vec()),
        _ => None,
    };
    let (Some(a), Some(b)) = (held(&key[li]), held(&next[li])) else {
        return Ok(());
    };
    if collator.cmp(&a, &b) != key[li].cmp(&next[li]) {
        return Err(disagreed(table, column, &a, &b));
    }
    Ok(())
}

/// The refusal of two values glibc's `strxfrm_l` and `strcoll_l` order otherwise.
fn disagreed(table: &Table, column: usize, a: &[u8], b: &[u8]) -> Error {
    Error::new(format!(
        "{}: {} holds {:?} and {:?}, which glibc's strxfrm_l under collation \"{}\" keys in \
         another order than its strcoll_l compares them: no order on it is cut",
        table.name,
        table.columns[column].name,
        String::from_utf8_lossy(a),
        String::from_utf8_lossy(b),
        table.columns[column]
            .collation
            .as_deref()
            .unwrap_or("default"),
    ))
}

/// What each level of an order is cut under, as `Head::cut_under` records it: nothing where no
/// library orders it, or where this build opens none.
pub(crate) fn cut_under_of(table: &Table, order: &Order) -> Vec<String> {
    order
        .levels
        .iter()
        .map(|l| table.columns[l.column()].cut_under().unwrap_or_default())
        .collect()
}

/// An order's top page read, its head and its entries, refused as `cut_checked` refuses it.
pub fn top_read(table: &Table, order: &Order, top: &[u8]) -> Result<(Head, Vec<Entry>)> {
    let (head, entries) = crate::directory::decode_top(top)?;
    cut_checked(table, order, &head)?;
    Ok((head, entries))
}

/// Refuses, by name, an order a level of which was cut under other rules than this build reads it
/// under: a library collation of another version, or one this build opens none of. Its rows read
/// whole as before, and the file's other orders read and write as before.
pub fn cut_checked(table: &Table, order: &Order, head: &Head) -> Result<()> {
    for (li, l) in order.levels.iter().enumerate() {
        let column = &table.columns[l.column()];
        let had = head.cut_under.get(li).map_or("", String::as_str);
        if had.is_empty() && !column.collated() {
            continue;
        }
        let now = column.cut_under();
        if now.as_deref().ok() != Some(had) {
            let reads = match now {
                Ok(n) if n.is_empty() => "no collation".to_string(),
                Ok(n) => n,
                Err(e) => {
                    return Err(Error::new(format!(
                        "{}: the order {} was cut on {} under {had}: {e}",
                        table.name, order.name, column.name
                    )))
                }
            };
            return Err(Error::new(format!(
                "{}: the order {} was cut on {} under {}, and this warren-snail reads it under \
                 {reads}: re-cut it with `warren-snail recut <file> {}`",
                table.name,
                order.name,
                column.name,
                if had.is_empty() { "no collation" } else { had },
                table.name
            )));
        }
    }
    Ok(())
}

/// The leaves an order holds keyed in 128 bits, by their slots.
pub(crate) fn wide_slots(table: &Table, order: &Order) -> Vec<u32> {
    order
        .leaves(table)
        .iter()
        .enumerate()
        .filter(|(_, (_, leaf))| leaf.ty.is_wide() && leaf.nested.is_none())
        .map(|(s, _)| s as u32)
        .collect()
}

/// The month and text leaves an order holds, by their slots: a month column's one leaf, and each
/// leaf of text or bytes.
pub(crate) fn slots(table: &Table, order: &Order) -> (Vec<u32>, Vec<u32>) {
    let months = table
        .month_columns()
        .into_iter()
        .filter_map(|c| order.leaf_slot(table, c))
        .map(|s| s as u32)
        .collect();
    let texts = order
        .leaves(table)
        .iter()
        .enumerate()
        .filter(|(_, (_, leaf))| leaf.ty.is_byte_array() && leaf.nested.is_none())
        .map(|(s, _)| s as u32)
        .collect();
    (months, texts)
}

/// The head of an order's directory for `n` rows laid out on it: each leaf's unit from its
/// furthest-out place, the month and text leaves, and each text level's sublevels.
pub(crate) fn head_of(table: &Table, order: &Order, laid: &Laid, n: usize, format: u32) -> Head {
    let most = largest_place(&laid.keys, &laid.blocks, order, table, &laid.widths);
    let (month_slots, text_slots) = slots(table, order);
    Head {
        units: (0..laid.widths.0.len())
            .map(|s| laid.widths.unit(s, most[s], n) as u32)
            .collect(),
        month_slots,
        text_slots,
        level_text: laid.text_sublevels.clone(),
        height: 0,
        wide_slots: wide_slots(table, order),
        cut_under: cut_under_of(table, order),
        span_slots: crate::spans::slots(table, order),
        bound_slots: bound_slots(table, order, format),
        separated: format >= crate::file::FORMAT,
    }
}

/// Which of an order's held leaves are the first of a column keyed by its bytes, whose least and
/// greatest each entry keeps cut to 8 bytes in a snail of format 9.
pub(crate) fn bound_slots(table: &Table, order: &Order, format: u32) -> Vec<u32> {
    if format < crate::file::FORMAT {
        return Vec::new();
    }
    order
        .columns
        .iter()
        .filter(|&&c| bounded(table, c) && table.columns[c].orderer().is_ok())
        .filter_map(|&c| order.leaf_slot(table, c).map(|s| s as u32))
        .collect()
}

/// The bytes row `r` of a column of type `ty` is compared by: a composed value's key, a string's key
/// under its column's collation; none for a NULL.
pub(crate) fn compared_key(
    ty: ColumnType,
    orderer: Option<&crate::collations::Collator>,
    values: &Values,
    r: usize,
) -> Option<Vec<u8>> {
    match values {
        Values::Composed(x) => x[r].as_ref().map(|c| c.key.to_vec()),
        _ => values
            .text_value(r)
            .map(|h| crate::strings::column_key(ty, orderer, h.data()).into_owned()),
    }
}

/// Whether a column is keyed by its bytes, so that an entry bounds it by its key cut to 8 bytes.
pub(crate) fn bounded(table: &Table, c: usize) -> bool {
    let ty = table.columns[c].ty;
    ty.keyed_by_text() || ty.keyed_by_bytes() || table.columns[c].collated()
}

/// The head of an order that holds no row yet): a unit of one disk page, and each text level cut on
/// its first byte, until rows come.
pub(crate) fn empty_head(table: &Table, order: &Order, format: u32) -> Head {
    let (month_slots, text_slots) = slots(table, order);
    Head {
        units: vec![1; order.leaves(table).len()],
        month_slots,
        text_slots,
        level_text: level_kinds(table, order, format),
        height: 1,
        wide_slots: wide_slots(table, order),
        cut_under: cut_under_of(table, order),
        span_slots: crate::spans::slots(table, order),
        bound_slots: bound_slots(table, order, format),
        separated: format >= crate::file::FORMAT,
    }
}

/// What a write says of each leaf an order holds, before its blocks are written: its name and its
/// unit.
fn leaves_written(table: &Table, order: &Order, units: &[usize]) -> Vec<ColumnWritten> {
    order
        .leaves(table)
        .iter()
        .zip(units)
        .map(|((c, leaf), &k)| ColumnWritten {
            name: leaf.name(&table.columns[*c].name),
            k,
            dictionary_blocks: 0,
            data_pages: 0,
            disk_pages: 0,
            moved_blocks: 0,
            bytes: (leaf.ty.is_byte_array() && leaf.nested.is_none()).then_some(0),
        })
        .collect()
}

/// An order that holds no row yet: its top alone.
fn empty_order(
    w: &mut dyn Place,
    table: &Table,
    order: &Order,
    format: u32,
) -> Result<(Extent, Written)> {
    let head = empty_head(table, order, format);
    let top = w.place(&encode_top(&head, &[]))?;
    let units: Vec<usize> = head.units.iter().map(|&k| k as usize).collect();
    let columns = leaves_written(table, order, &units);
    Ok((
        top,
        Written {
            table: table.name.clone(),
            order: order.name.clone(),
            rows: 0,
            blocks: 0,
            index_pages: top.pages(),
            levels: head.height,
            map_pages: 0,
            columns,
        },
    ))
}

/// The descriptor of each leaf an order holds, by its slot.
pub(crate) fn leaf_descriptors(
    table: &Table,
    order: &Order,
) -> Result<Vec<parquet::schema::types::ColumnDescPtr>> {
    order
        .leaves(table)
        .iter()
        .map(|(c, leaf)| {
            let col = &table.columns[*c];
            leaf_descriptor(&col.name, leaf, !col.not_null)
        })
        .collect()
}

fn write_order(
    w: &mut dyn Place,
    table: &Table,
    order: &Order,
    rows: &Rows,
    source: Option<&Source>,
    packed: bool,
    format: u32,
) -> Result<(Extent, Written)> {
    let n = rows.len();
    let Laid {
        perm,
        keys,
        text_sublevels,
        widths,
        blocks,
    } = laid_out(table, order, rows, None, format)?;
    let most = largest_place(&keys, &blocks, order, table, &widths);
    let units: Vec<usize> = (0..widths.0.len())
        .map(|s| widths.unit(s, most[s], n))
        .collect();
    // In format 9 each block takes the unit of its own furthest-out place.
    let kept = format >= crate::file::FORMAT;
    let block_units: Vec<Vec<usize>> = if kept {
        largest_places(&keys, &blocks, order, table, &widths)
            .iter()
            .map(|m| {
                (0..widths.0.len())
                    .map(|s| widths.unit(s, m[s], n))
                    .collect()
            })
            .collect()
    } else {
        vec![units.clone(); blocks.len()]
    };
    let descs = leaf_descriptors(table, order)?;
    let (month_slots, text_slots) = slots(table, order);
    let month_slots: Vec<usize> = month_slots.into_iter().map(|s| s as usize).collect();
    let text_slots: Vec<usize> = text_slots.into_iter().map(|s| s as usize).collect();

    let mut entries = Vec::with_capacity(blocks.len());
    let mut columns = leaves_written(table, order, &units);
    let mut maps = MapPages::new(packed);
    // The rows came in the order's order, so a row group can be one of its blocks.
    let in_order = source.filter(|_| perm.iter().enumerate().all(|(i, &p)| i == p));
    // Where each leaf the order holds lies among the table's, as a source's chunks lie.
    let at: Vec<usize> = order
        .columns
        .iter()
        .flat_map(|&c| {
            let first = table.first_leaf(c);
            first..first + table.leaf_count(c)
        })
        .collect();
    for (bi, b) in blocks.iter().enumerate() {
        let units = &block_units[bi];
        let given: Vec<Option<Encoded>> = match in_order.and_then(|s| {
            s.groups
                .iter()
                .position(|&(first, n)| first == b.start && n == b.rows())
                .map(|g| &s.chunks[g])
        }) {
            Some(chunks) => at
                .iter()
                .enumerate()
                .map(|(s, &l)| {
                    chunks[l]
                        .as_ref()
                        .filter(|e| {
                            e.dictionary
                                .as_ref()
                                .is_none_or(|d| d.len() <= DICTIONARY_BYTES)
                                && e.pages
                                    .iter()
                                    .all(|p| disk_pages(p.bytes.len()) <= units[s])
                        })
                        .cloned()
                })
                .collect(),
            None => Vec::new(),
        };
        let (entry, encoded, moved) = write_block(
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
            &given,
            &mut maps,
            kept,
        )?;
        for (s, (cw, e)) in columns.iter_mut().zip(&encoded).enumerate() {
            cw.moved_blocks += moved[s] as usize;
            cw.dictionary_blocks += e.dictionary.is_some() as usize;
            cw.data_pages += e.pages.len();
            cw.disk_pages += e.disk_pages();
        }
        for (&s, &bytes) in text_slots.iter().zip(&entry.bytes) {
            *columns[s].bytes.as_mut().unwrap() += bytes as u64;
        }
        entries.push(entry);
    }
    if kept {
        crate::directory::separate(&mut entries, &text_sublevels, None, None);
    }
    maps.finish(w)?;
    let map_pages = maps.pages;
    let head = Head {
        units: units.iter().map(|&k| k as u32).collect(),
        month_slots: month_slots.iter().map(|&s| s as u32).collect(),
        text_slots: text_slots.iter().map(|&s| s as u32).collect(),
        level_text: text_sublevels.clone(),
        height: 0,
        wide_slots: wide_slots(table, order),
        cut_under: cut_under_of(table, order),
        span_slots: crate::spans::slots(table, order),
        bound_slots: bound_slots(table, order, format),
        separated: format >= crate::file::FORMAT,
    };
    let (top, below) = build(head, entries, &mut |bytes| w.place(bytes))?;
    let levels = decode_top(&top)?.0.height;
    let extent = w.place(&top)?;
    Ok((
        extent,
        Written {
            table: table.name.clone(),
            order: order.name.clone(),
            rows: n,
            blocks: blocks.len(),
            index_pages: below + extent.pages(),
            levels,
            map_pages,
            columns,
        },
    ))
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_block(
    w: &mut dyn Place,
    table: &Table,
    order: &Order,
    rows: &Rows,
    perm: &[usize],
    keys: &Keys,
    b: &Block,
    descs: &[parquet::schema::types::ColumnDescPtr],
    units: &[usize],
    month_slots: &[usize],
    text_slots: &[usize],
    given: &[Option<Encoded>],
    maps: &mut MapPages,
    current: bool,
) -> Result<(Entry, Vec<Encoded>, Vec<bool>)> {
    let leaves = order.leaves(table);
    let mut summaries = Vec::with_capacity(leaves.len());
    let mut encoded = Vec::with_capacity(leaves.len());
    let mut moved = Vec::with_capacity(leaves.len());
    let mut months = vec![[0u32; 12]; month_slots.len()];
    let mut ends = vec![[0u32; 2]; month_slots.len()];
    let mut bytes = vec![0u32; text_slots.len()];
    let spans: Vec<crate::directory::Span> = crate::spans::columns(table, order)
        .into_iter()
        .map(|c| {
            crate::spans::of_rows(table.columns[c].ty, &rows.columns[c], &perm[b.start..b.end])
        })
        .collect();
    let block_leaves = order
        .columns
        .iter()
        .flat_map(|&c| table.split_leaves(c, rows.columns[c].gather(&perm[b.start..b.end])));
    for (s, ((_, leaf), values)) in leaves.iter().zip(block_leaves).enumerate() {
        let present: Vec<i64> = values
            .ints()
            .map_or(Vec::new(), |v| v.iter().flatten().copied().collect());
        let keyed: Vec<i128> = (0..values.len()).filter_map(|i| values.wide(i)).collect();
        summaries.push(Summary {
            nulls: values.nulls() as u32,
            min: keyed.iter().copied().min().unwrap_or(0),
            max: keyed.iter().copied().max().unwrap_or(0),
            pages: 0,
            wide: leaf.ty.is_wide() && leaf.nested.is_none(),
        });
        if let Some(m) = month_slots.iter().position(|&x| x == s) {
            for &v in &present {
                match month_of(v) {
                    Some(month) => months[m][month as usize - 1] += 1,
                    None => ends[m][(v > 0) as usize] += 1,
                }
            }
        }
        if let Some(t) = text_slots.iter().position(|&x| x == s) {
            bytes[t] = u32::try_from(values.bytes()).map_err(|_| {
                Error::new(format!(
                    "{}: a block holds more text than its count",
                    table.name
                ))
            })?;
        }
        // A leaf given as a file's pages comes in as they lie where the counts would choose its
        // encoding, a dictionary or none; otherwise it is encoded afresh.
        let fresh = encode(&descs[s], leaf.ty, &values, units[s])?;
        match given.get(s).and_then(Option::as_ref) {
            Some(e) if e.dictionary.is_some() == fresh.dictionary.is_some() => {
                encoded.push(e.clone());
                moved.push(true);
            }
            _ => {
                encoded.push(fresh);
                moved.push(false);
            }
        }
    }
    // In format 9, a column keyed by its bytes: its least and greatest key, cut to 8 bytes, in its
    // first leaf's summary.
    if current {
        for &c in order.columns.iter().filter(|&&c| bounded(table, c)) {
            let Ok(orderer) = table.columns[c].orderer() else {
                continue;
            };
            let ty = table.columns[c].ty;
            let bounds = perm[b.start..b.end]
                .iter()
                .filter_map(|&r| compared_key(ty, orderer, &rows.columns[c], r))
                .map(|k| crate::directory::prefix_bound(&k));
            if let Some((lo, hi)) = bounds.fold(None, |m: Option<(i128, i128)>, v| {
                Some(m.map_or((v, v), |(lo, hi)| (lo.min(v), hi.max(v))))
            }) {
                let s = order.leaf_slot(table, c).unwrap();
                summaries[s].min = lo;
                summaries[s].max = hi;
            }
        }
    }
    let map_at = |start: u64| -> PageMap {
        let mut at = start;
        let mut place = |bytes: usize| {
            let e = Extent {
                start: at,
                bytes: bytes as u64,
            };
            at += disk_pages(bytes) as u64;
            e
        };
        PageMap {
            columns: encoded
                .iter()
                .enumerate()
                .map(|(s, e)| ColumnPages {
                    unit: if current { units[s] as u32 } else { 0 },
                    dictionary: e.dictionary.as_ref().map(|d| place(d.len())),
                    pages: e
                        .pages
                        .iter()
                        .map(|p| PageEntry {
                            at: place(p.bytes.len()),
                            first_row: p.first_row as u32,
                            rows: p.rows as u32,
                            first: keys.row(b.start + p.first_row).to_vec(),
                        })
                        .collect(),
                })
                .collect(),
            sorted: b.rows() as u32,
            dead: Vec::new(),
            chains: Vec::new(),
        }
    };
    let map_bytes = map_at(0).encode().len();
    let data_pages: usize = encoded.iter().map(Encoded::disk_pages).sum();
    let (child, data) = maps.place(w, map_bytes, data_pages as u64, true)?;
    let map = map_at(data);
    for (sum, cp) in summaries.iter_mut().zip(&map.columns) {
        sum.pages = (cp.dictionary.map_or(0, Extent::pages)
            + cp.pages.iter().map(|p| p.at.pages()).sum::<u64>()) as u32;
    }
    let map_encoded = map.encode();
    debug_assert_eq!(map_encoded.len(), map_bytes);
    maps.write(w, child, &map_encoded)?;
    for (e, cp) in encoded.iter().zip(&map.columns) {
        if let (Some(d), Some(at)) = (&e.dictionary, cp.dictionary) {
            w.write_at(at.start, d)?;
        }
        for (p, pe) in e.pages.iter().zip(&cp.pages) {
            w.write_at(pe.at.start, &p.bytes)?;
        }
    }
    let entry = Entry {
        open: false,
        depth: b.place.len() as u32,
        rows: b.rows() as u32,
        dead: 0,
        first: keys.row(b.start).to_vec(),
        last: keys.row(b.end - 1).to_vec(),
        columns: summaries,
        months,
        ends,
        bytes,
        spans,
        child,
    };
    Ok((entry, encoded, moved))
}
