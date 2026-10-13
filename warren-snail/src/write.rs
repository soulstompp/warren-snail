//! A write from Arrow record batches: each row routed by its places onto the table's order and onto
//! each index, the blocks cut and encoded, every block's pages written, then the directory, then
//! the root.

use crate::catalog::{Catalog, Column, ColumnType, Order, Table};
use crate::cut::{cut, largest_place, unit, Block, Width, Widths};
use crate::directory::{
    build, decode_top, encode_top, ColumnPages, Entry, Head, MapPages, PageEntry, PageMap, Summary,
};
use crate::encode::{descriptor, disk_pages, encode, DataPage, Encoded, DICTIONARY_BYTES};
use crate::file::{encode_free, Extent, Place, Root, Run, Writer, FILE_PAGES, FORMAT_SEVEN};
use crate::places::{key, month_utc, shown, KeyValue, Keys, Sublevels};
use crate::values::Values;
use crate::{Error, Result};
use arrow::array::{
    Array, BinaryArray, BinaryViewArray, Decimal128Array, Int16Array, Int32Array, Int64Array,
    LargeBinaryArray, LargeStringArray, RecordBatch, StringArray, StringViewArray,
    TimestampMicrosecondArray,
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
        for c in &table.columns {
            let mut out = Values::new(c.ty);
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
                        ColumnType::Timestamptz,
                        DataType::Timestamp(TimeUnit::Microsecond, _),
                    ) => {
                        let a = a
                            .as_any()
                            .downcast_ref::<TimestampMicrosecondArray>()
                            .unwrap();
                        out.extend(a.iter());
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
                    (
                        Values::Int(out),
                        ColumnType::Numeric { precision, scale },
                        DataType::Decimal128(_, s),
                    ) => {
                        let a = a.as_any().downcast_ref::<Decimal128Array>().unwrap();
                        let s = *s as i32;
                        let to = scale as i32;
                        let limit = 10i128.pow(precision as u32);
                        for v in a.iter() {
                            out.push(match v {
                                None => None,
                                Some(x) => {
                                    // A value at a scale finer than the column's is refused
                                    // unless its extra digits are zeros.
                                    let units = if s <= to {
                                        x.checked_mul(10i128.pow((to - s) as u32))
                                    } else {
                                        let d = 10i128.pow((s - to) as u32);
                                        (x % d == 0).then_some(x / d)
                                    }
                                    .filter(|u| u.abs() < limit)
                                    .ok_or_else(|| {
                                        Error::new(format!(
                                            "{}.{}: a value is no {}",
                                            table.name,
                                            c.name,
                                            c.ty.name()
                                        ))
                                    })?;
                                    Some(units as i64)
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

/// What a write laid out, order by order.
#[derive(Debug)]
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
/// row group and each of the table's columns, the chunk's pages where they can come in as they lie.
pub struct Source {
    pub groups: Vec<(usize, usize)>,
    pub chunks: Vec<Vec<Option<Encoded>>>,
}

/// The row groups of a Parquet file holding a table's rows, and the pages of each column chunk that
/// can come in as they lie: uncompressed version 1 pages, plain or on a dictionary, each column of
/// the table's own type.
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
    let at: Vec<Option<usize>> = table
        .columns
        .iter()
        .map(|c| {
            (0..schema.num_columns()).find(|&i| schema.column(i).path().parts() == [c.name.clone()])
        })
        .collect();
    let mut groups = Vec::new();
    let mut chunks = Vec::new();
    let mut first = 0;
    for (g, rg) in meta.row_groups().iter().enumerate() {
        let n = rg.num_rows() as usize;
        groups.push((first, n));
        first += n;
        let mut row = Vec::with_capacity(table.columns.len());
        for (c, col) in table.columns.iter().enumerate() {
            let offsets = meta
                .offset_index()
                .and_then(|o| o.get(g))
                .and_then(|o| at[c].and_then(|i| o.get(i)));
            row.push(match (at[c], offsets) {
                (Some(i), Some(o)) => movable(&file, rg.column(i), o, col, n)?,
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

/// A column chunk's pages, where they can come in as they lie.
fn movable(
    file: &std::fs::File,
    chunk: &parquet::file::metadata::ColumnChunkMetaData,
    offsets: &parquet::file::page_index::offset_index::OffsetIndexMetaData,
    col: &Column,
    rows: usize,
) -> Result<Option<Encoded>> {
    use parquet::basic::{Compression, Encoding, PageType};
    use std::os::unix::fs::FileExt;
    let ours = descriptor(&col.name, col.ty, !col.not_null)?;
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
    write_made(path, catalog, tables, sources, span, None)
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
    write_made(path, catalog, tables, &[], span, Some(FORMAT_SEVEN))
}

fn write_made(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
    sources: &[(String, Source)],
    span: u64,
    format: Option<u32>,
) -> Result<Vec<Written>> {
    for (name, _) in tables {
        if catalog.table(name).is_none() {
            return Err(Error::new(format!("{name} is not in the DDL")));
        }
    }
    let mut w = Writer::create_spanned(path, span)?;
    if let Some(f) = format {
        w.set_format(f);
    }
    let written = write_tables(w, catalog, tables, sources);
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
) -> Result<Vec<Written>> {
    let mut root = Root {
        generation: 1,
        ..Default::default()
    };
    let mut out = Vec::new();
    for table in &catalog.tables {
        let Some(rows) = tables
            .iter()
            .find(|(n, _)| *n == table.name)
            .map(|(_, r)| r)
        else {
            for order in &table.orders {
                let (extent, written) = empty_order(&mut w, table, order)?;
                root.orders
                    .push((table.name.clone(), order.name.clone(), extent));
                out.push(written);
            }
            continue;
        };
        for (c, values) in table.columns.iter().zip(&rows.columns) {
            if values.len() != rows.len() {
                return Err(Error::new(format!(
                    "{}: its columns differ in length",
                    table.name
                )));
            }
            if c.not_null && values.nulls() > 0 {
                return Err(Error::new(format!(
                    "{}.{} is NOT NULL and a row holds NULL",
                    table.name, c.name
                )));
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
        for c in table.month_columns() {
            if rows.columns[c]
                .ints()
                .is_some_and(|v| v.iter().flatten().any(|&x| x == i64::MAX || x == i64::MIN))
            {
                return Err(Error::new(format!(
                    "{}.{} holds an infinite instant, whose month cannot be counted",
                    table.name, table.columns[c].name
                )));
            }
        }
        let source = sources
            .iter()
            .find(|(n, _)| *n == table.name)
            .map(|(_, s)| s);
        for order in &table.orders {
            let packed = w.format() != FORMAT_SEVEN;
            let (extent, written) = write_order(&mut w, table, order, rows, source, packed)?;
            root.orders
                .push((table.name.clone(), order.name.clone(), extent));
            out.push(written);
        }
    }
    root.catalog = w.place(catalog.ddl.as_bytes())?;
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
) -> Result<Laid> {
    let n = rows.len();
    let stride = order.levels.len();
    let mut unsorted: Vec<KeyValue> = Vec::with_capacity(n * stride);
    // For each level, the sublevels a text level is cut on: its longest value's bytes and one more.
    let mut text_sublevels = vec![0u32; stride];
    let fixed = given.is_some();
    if let Some(g) = given {
        text_sublevels = g.to_vec();
    }
    for r in 0..n {
        for (li, &l) in order.levels.iter().enumerate() {
            let null = || {
                Error::new(format!(
                    "{}: a level of {} holds NULL",
                    table.name, order.name
                ))
            };
            let column = &rows.columns[l.column()];
            let v = match column {
                Values::Text(_) | Values::Utf8(_) | Values::Dict { .. } => {
                    let s = column.text(r).ok_or_else(null)?;
                    if !fixed {
                        text_sublevels[li] = text_sublevels[li].max(s.len() as u32 + 1);
                    }
                    unsorted.push(KeyValue::Text(s.into()));
                    continue;
                }
                Values::Int(v) => v[r].ok_or_else(null)?,
            };
            if table.columns[l.column()].ty == ColumnType::Timestamptz
                && (v == i64::MAX || v == i64::MIN)
            {
                return Err(Error::new(format!(
                    "{}: {} holds an infinite instant, which a level of {} cannot place",
                    table.name,
                    table.columns[l.column()].name,
                    order.name
                )));
            }
            unsorted.push(key(l, v));
        }
    }
    let row = |r: usize| &unsorted[r * stride..(r + 1) * stride];
    let mut perm: Vec<usize> = (0..n).collect();
    perm.sort_by(|&a, &b| row(a).cmp(row(b)));
    if order.unique {
        if let Some(p) = perm.windows(2).find(|p| row(p[0]) == row(p[1])) {
            return Err(Error::new(format!(
                "{}: two rows share the value {} of {}, which is unique",
                table.name,
                shown(row(p[0])),
                order.name
            )));
        }
    }
    let keys = Keys {
        stride,
        data: perm.iter().flat_map(|&r| row(r).iter().cloned()).collect(),
    };
    let sublevels = Sublevels::new(order, table, &text_sublevels);
    let widths = Widths(
        order
            .columns
            .iter()
            .map(|&c| {
                let ty = table.columns[c].ty;
                match ty.width() {
                    Some(wd) => Width::Fixed(wd),
                    None => {
                        let mut sum = Vec::with_capacity(n + 1);
                        sum.push(0u64);
                        for &r in &perm {
                            sum.push(sum.last().unwrap() + rows.columns[c].plain(r, ty));
                        }
                        Width::Varying(sum)
                    }
                }
            })
            .collect(),
    );
    let blocks = cut(&keys, &sublevels, &widths);
    Ok(Laid {
        perm,
        keys,
        text_sublevels,
        widths,
        blocks,
    })
}

/// The month and text columns an order holds, by their slots.
fn slots(table: &Table, order: &Order) -> (Vec<u32>, Vec<u32>) {
    let months = table
        .month_columns()
        .into_iter()
        .filter_map(|c| order.slot(c))
        .map(|s| s as u32)
        .collect();
    let texts = (0..order.columns.len())
        .filter(|&s| table.columns[order.columns[s]].ty.is_byte_array())
        .map(|s| s as u32)
        .collect();
    (months, texts)
}

/// The head of an order's directory for `n` rows laid out on it: each column's unit from its
/// furthest-out place, the month and text columns, and each text level's sublevels.
pub(crate) fn head_of(table: &Table, order: &Order, laid: &Laid, n: usize) -> Head {
    let most = largest_place(&laid.keys, &laid.blocks, order, table, &laid.widths);
    let (month_slots, text_slots) = slots(table, order);
    Head {
        units: (0..laid.widths.0.len())
            .map(|s| unit(most[s] as usize, laid.widths.per_row(s, n)) as u32)
            .collect(),
        month_slots,
        text_slots,
        level_text: laid.text_sublevels.clone(),
        height: 0,
    }
}

/// The head of an order that holds no row yet: a unit of one disk page, and each text level cut on
/// its first byte, until rows come.
pub(crate) fn empty_head(table: &Table, order: &Order) -> Head {
    let (month_slots, text_slots) = slots(table, order);
    Head {
        units: vec![1; order.columns.len()],
        month_slots,
        text_slots,
        level_text: order
            .levels
            .iter()
            .map(|l| table.columns[l.column()].ty.is_byte_array() as u32)
            .collect(),
        height: 1,
    }
}

/// An order that holds no row yet: its top alone.
fn empty_order(w: &mut dyn Place, table: &Table, order: &Order) -> Result<(Extent, Written)> {
    let head = empty_head(table, order);
    let top = w.place(&encode_top(&head, &[]))?;
    let columns = order
        .columns
        .iter()
        .zip(&head.units)
        .map(|(&c, &k)| ColumnWritten {
            name: table.columns[c].name.clone(),
            k: k as usize,
            dictionary_blocks: 0,
            data_pages: 0,
            disk_pages: 0,
            moved_blocks: 0,
            bytes: table.columns[c].ty.is_byte_array().then_some(0),
        })
        .collect();
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

fn write_order(
    w: &mut dyn Place,
    table: &Table,
    order: &Order,
    rows: &Rows,
    source: Option<&Source>,
    packed: bool,
) -> Result<(Extent, Written)> {
    let n = rows.len();
    let Laid {
        perm,
        keys,
        text_sublevels,
        widths,
        blocks,
    } = laid_out(table, order, rows, None)?;
    let most = largest_place(&keys, &blocks, order, table, &widths);
    let units: Vec<usize> = (0..widths.0.len())
        .map(|s| unit(most[s] as usize, widths.per_row(s, n)))
        .collect();
    let descs = order
        .columns
        .iter()
        .map(|&c| {
            let col = &table.columns[c];
            descriptor(&col.name, col.ty, !col.not_null)
        })
        .collect::<Result<Vec<_>>>()?;
    let month_slots: Vec<usize> = table
        .month_columns()
        .into_iter()
        .filter_map(|c| order.slot(c))
        .collect();
    let text_slots: Vec<usize> = (0..order.columns.len())
        .filter(|&s| table.columns[order.columns[s]].ty.is_byte_array())
        .collect();

    let mut entries = Vec::with_capacity(blocks.len());
    let mut columns: Vec<ColumnWritten> = order
        .columns
        .iter()
        .zip(&units)
        .map(|(&c, &k)| ColumnWritten {
            name: table.columns[c].name.clone(),
            k,
            dictionary_blocks: 0,
            data_pages: 0,
            disk_pages: 0,
            moved_blocks: 0,
            bytes: table.columns[c].ty.is_byte_array().then_some(0),
        })
        .collect();
    let mut maps = MapPages::new(packed);
    // The rows came in the order's order, so a row group can be one of its blocks.
    let in_order = source.filter(|_| perm.iter().enumerate().all(|(i, &p)| i == p));
    for b in &blocks {
        let given: Vec<Option<Encoded>> = match in_order.and_then(|s| {
            s.groups
                .iter()
                .position(|&(first, n)| first == b.start && n == b.rows())
                .map(|g| &s.chunks[g])
        }) {
            Some(chunks) => order
                .columns
                .iter()
                .enumerate()
                .map(|(s, &c)| {
                    chunks[c]
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
            &units,
            &month_slots,
            &text_slots,
            &given,
            &mut maps,
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
    maps.finish(w)?;
    let map_pages = maps.pages;
    let head = Head {
        units: units.iter().map(|&k| k as u32).collect(),
        month_slots: month_slots.iter().map(|&s| s as u32).collect(),
        text_slots: text_slots.iter().map(|&s| s as u32).collect(),
        level_text: text_sublevels.clone(),
        height: 0,
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
) -> Result<(Entry, Vec<Encoded>, Vec<bool>)> {
    let mut summaries = Vec::with_capacity(order.columns.len());
    let mut encoded = Vec::with_capacity(order.columns.len());
    let mut moved = Vec::with_capacity(order.columns.len());
    let mut months = vec![[0u32; 12]; month_slots.len()];
    let mut bytes = vec![0u32; text_slots.len()];
    for (s, &c) in order.columns.iter().enumerate() {
        let values = rows.columns[c].gather(&perm[b.start..b.end]);
        let present: Vec<i64> = values
            .ints()
            .map_or(Vec::new(), |v| v.iter().flatten().copied().collect());
        summaries.push(Summary {
            nulls: values.nulls() as u32,
            min: present.iter().copied().min().unwrap_or(0),
            max: present.iter().copied().max().unwrap_or(0),
            pages: 0,
        });
        if let Some(m) = month_slots.iter().position(|&x| x == s) {
            for &v in &present {
                months[m][month_utc(v) as usize - 1] += 1;
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
        // A column given as a file's pages comes in as they lie where the counts would choose its
        // encoding, a dictionary or none; otherwise it is encoded afresh.
        let fresh = encode(&descs[s], table.columns[c].ty, &values, units[s])?;
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
                .map(|e| ColumnPages {
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
        depth: b.place.len() as u32,
        rows: b.rows() as u32,
        dead: 0,
        first: keys.row(b.start).to_vec(),
        last: keys.row(b.end - 1).to_vec(),
        columns: summaries,
        months,
        bytes,
        child,
    };
    Ok((entry, encoded, moved))
}
