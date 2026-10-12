//! A block's column as the plain parquet library writes it: its own data pages, page by page.

use crate::catalog::ColumnType;
use crate::cut::{DATA_PAGE_ROWS, PAGE};
use crate::values::{Values, LENGTH_PREFIX};
use crate::{Error, Result};
use bytes::Bytes;
use parquet::basic::{
    Compression, Encoding, LogicalType, Repetition, TimeUnit, Type as PhysicalType,
};
use parquet::column::page::{Page, PageReader};
use parquet::column::reader::{get_column_reader, get_typed_column_reader};
use parquet::column::writer::{ColumnCloseResult, ColumnWriterImpl};
use parquet::data_type::{ByteArray, ByteArrayType, DataType, Int32Type, Int64Type};
use parquet::file::metadata::ColumnChunkMetaData;
use parquet::file::properties::{WriterProperties, WriterVersion};
use parquet::file::serialized_reader::SerializedPageReader;
use parquet::file::writer::{SerializedPageWriter, TrackedWrite};
use parquet::schema::types::{ColumnDescPtr, ColumnDescriptor, ColumnPath, Type};
use std::collections::HashSet;
use std::sync::Arc;

/// The dictionary page the plain library allows, at most.
pub(crate) const DICTIONARY_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct DataPage {
    pub bytes: Vec<u8>,
    pub first_row: usize,
    pub rows: usize,
}

#[derive(Clone, Debug)]
pub struct Encoded {
    pub dictionary: Option<Vec<u8>>,
    pub pages: Vec<DataPage>,
}

pub fn disk_pages(bytes: usize) -> usize {
    bytes.div_ceil(PAGE)
}

impl Encoded {
    /// The disk pages the column takes, each Parquet page laid on whole disk pages.
    pub fn disk_pages(&self) -> usize {
        self.dictionary.as_ref().map_or(0, |d| disk_pages(d.len()))
            + self
                .pages
                .iter()
                .map(|p| disk_pages(p.bytes.len()))
                .sum::<usize>()
    }
}

pub fn descriptor(name: &str, ty: ColumnType, nullable: bool) -> Result<ColumnDescPtr> {
    let (physical, logical) = match ty {
        ColumnType::Int2 => (
            PhysicalType::INT32,
            Some(LogicalType::Integer {
                bit_width: 16,
                is_signed: true,
            }),
        ),
        ColumnType::Int4 => (
            PhysicalType::INT32,
            Some(LogicalType::Integer {
                bit_width: 32,
                is_signed: true,
            }),
        ),
        ColumnType::Int8 => (
            PhysicalType::INT64,
            Some(LogicalType::Integer {
                bit_width: 64,
                is_signed: true,
            }),
        ),
        ColumnType::Timestamptz => (
            PhysicalType::INT64,
            Some(LogicalType::Timestamp {
                is_adjusted_to_u_t_c: true,
                unit: TimeUnit::MICROS,
            }),
        ),
        ColumnType::Text(_) => (PhysicalType::BYTE_ARRAY, Some(LogicalType::String)),
        ColumnType::Bytes => (PhysicalType::BYTE_ARRAY, None),
        ColumnType::Numeric { precision, scale } => (
            PhysicalType::INT64,
            Some(LogicalType::Decimal {
                scale: scale as i32,
                precision: precision as i32,
            }),
        ),
    };
    let repetition = if nullable {
        Repetition::OPTIONAL
    } else {
        Repetition::REQUIRED
    };
    let mut t = Type::primitive_type_builder(name, physical)
        .with_repetition(repetition)
        .with_logical_type(logical);
    if let ColumnType::Numeric { precision, scale } = ty {
        t = t.with_precision(precision as i32).with_scale(scale as i32);
    }
    let t = t.build()?;
    Ok(Arc::new(ColumnDescriptor::new(
        Arc::new(t),
        if nullable { 1 } else { 0 },
        0,
        ColumnPath::new(vec![name.to_string()]),
    )))
}

fn props(dictionary: bool, rows_per_page: usize) -> Arc<WriterProperties> {
    Arc::new(
        WriterProperties::builder()
            .set_writer_version(WriterVersion::PARQUET_1_0)
            .set_compression(Compression::UNCOMPRESSED)
            .set_dictionary_enabled(dictionary)
            .set_dictionary_page_size_limit(usize::MAX)
            .set_data_page_size_limit(usize::MAX)
            .set_data_page_row_count_limit(rows_per_page)
            .set_write_batch_size(rows_per_page)
            .build(),
    )
}

fn write_typed<T: DataType, V>(
    desc: &ColumnDescPtr,
    props: Arc<WriterProperties>,
    values: &[Option<V>],
    rows_per_page: usize,
    conv: impl Fn(&V) -> T::T,
) -> Result<(Vec<u8>, ColumnCloseResult)> {
    let mut sink = TrackedWrite::new(Vec::new());
    let closed = {
        let pages = Box::new(SerializedPageWriter::new(&mut sink));
        let mut w = ColumnWriterImpl::<T>::new(desc.clone(), props, pages);
        let nullable = desc.max_def_level() > 0;
        for chunk in values.chunks(rows_per_page) {
            let present: Vec<T::T> = chunk.iter().flatten().map(&conv).collect();
            if nullable {
                let defs: Vec<i16> = chunk.iter().map(|v| v.is_some() as i16).collect();
                w.write_batch(&present, Some(&defs), None)?;
            } else {
                w.write_batch(&present, None, None)?;
            }
        }
        w.close()?
    };
    Ok((sink.into_inner()?, closed))
}

fn write(
    desc: &ColumnDescPtr,
    values: &Values,
    dictionary: bool,
    rows_per_page: usize,
) -> Result<Encoded> {
    if values.nulls() > 0 && desc.max_def_level() == 0 {
        return Err(Error::new(format!(
            "{} holds a NULL and is NOT NULL",
            desc.name()
        )));
    }
    let p = props(dictionary, rows_per_page);
    let (bytes, closed) = match (values, desc.physical_type()) {
        (Values::Int(v), PhysicalType::INT32) => {
            write_typed::<Int32Type, i64>(desc, p, v, rows_per_page, |&x| x as i32)?
        }
        (Values::Int(v), PhysicalType::INT64) => {
            write_typed::<Int64Type, i64>(desc, p, v, rows_per_page, |&x| x)?
        }
        (Values::Text(v), PhysicalType::BYTE_ARRAY) => {
            write_typed::<ByteArrayType, String>(desc, p, v, rows_per_page, |s| {
                ByteArray::from(s.as_str())
            })?
        }
        (Values::Utf8(v), PhysicalType::BYTE_ARRAY) => {
            write_typed::<ByteArrayType, ByteArray>(desc, p, v, rows_per_page, |b| b.clone())?
        }
        _ => {
            return Err(Error::new(format!(
                "{}: its values are not of its type",
                desc.name()
            )))
        }
    };
    let rows = values.len();
    let locations = closed
        .offset_index
        .as_ref()
        .ok_or_else(|| Error::new("the library wrote no offset index"))?
        .page_locations()
        .clone();
    let dictionary = closed
        .metadata
        .dictionary_page_offset()
        .map(|at| bytes[at as usize..locations[0].offset as usize].to_vec());
    let mut pages = Vec::with_capacity(locations.len());
    for (i, l) in locations.iter().enumerate() {
        let first = l.first_row_index as usize;
        let next = locations
            .get(i + 1)
            .map_or(rows, |n| n.first_row_index as usize);
        let at = l.offset as usize;
        pages.push(DataPage {
            bytes: bytes[at..at + l.compressed_page_size as usize].to_vec(),
            first_row: first,
            rows: next - first,
        });
    }
    Ok(Encoded { dictionary, pages })
}

/// The most rows a data page of `k` disk pages can hold, first guessed from the bits a value takes.
fn guess(k: usize, bits: usize, nullable: bool) -> usize {
    let room = (k * PAGE).saturating_sub(256) * 8;
    (room / (bits + nullable as usize).max(1)).clamp(1, DATA_PAGE_ROWS)
}

/// Writes with fewer rows a page until every data page lies within `k` disk pages.
fn fitted(
    desc: &ColumnDescPtr,
    values: &Values,
    dictionary: bool,
    k: usize,
    mut rows: usize,
) -> Result<Encoded> {
    let limit = k * PAGE;
    for _ in 0..64 {
        let e = write(desc, values, dictionary, rows)?;
        let largest = e.pages.iter().map(|p| p.bytes.len()).max().unwrap_or(0);
        if largest <= limit {
            return Ok(e);
        }
        let next = rows * limit / largest;
        rows = if next >= rows { rows - 1 } else { next.max(1) };
    }
    Err(Error::new(format!(
        "{}: no data page fits {k} disk pages",
        desc.name()
    )))
}

/// The bits a value takes written plain: a number its width, text its bytes and its length on the
/// mean of the values present.
fn plain_bits(values: &Values, ty: ColumnType) -> usize {
    match ty.width() {
        Some(w) => w * 8,
        None => {
            let present = (values.len() - values.nulls()).max(1) as u64;
            ((values.bytes() + LENGTH_PREFIX * present).div_ceil(present) * 8) as usize
        }
    }
}

/// The distinct values present, and the bytes a dictionary of them takes written plain.
fn distinct(values: &Values, ty: ColumnType) -> (usize, usize) {
    match values {
        Values::Int(v) => {
            let d: HashSet<i64> = v.iter().flatten().copied().collect();
            (d.len(), d.len() * ty.width().unwrap_or(8))
        }
        Values::Text(v) => {
            let d: HashSet<&str> = v.iter().flatten().map(String::as_str).collect();
            (
                d.len(),
                d.iter().map(|s| s.len() + LENGTH_PREFIX as usize).sum(),
            )
        }
        Values::Utf8(v) => {
            let d: HashSet<&[u8]> = v.iter().flatten().map(|b| b.data()).collect();
            (
                d.len(),
                d.iter().map(|s| s.len() + LENGTH_PREFIX as usize).sum(),
            )
        }
        Values::Dict { dict, codes } => {
            let d: HashSet<&[u8]> = codes
                .iter()
                .flatten()
                .map(|&c| dict[c as usize].data())
                .collect();
            (
                d.len(),
                d.iter().map(|s| s.len() + LENGTH_PREFIX as usize).sum(),
            )
        }
    }
}

/// A block's column, each data page within `k` disk pages: as plain values, or with a dictionary
/// where that takes fewer disk pages.
pub fn encode(desc: &ColumnDescPtr, ty: ColumnType, values: &Values, k: usize) -> Result<Encoded> {
    let nullable = desc.max_def_level() > 0;
    let plain = fitted(
        desc,
        values,
        false,
        k,
        guess(k, plain_bits(values, ty), nullable),
    )?;
    let (count, bytes) = distinct(values, ty);
    if count == 0 || bytes > DICTIONARY_BYTES {
        return Ok(plain);
    }
    let bits = (usize::BITS - (count.max(2) - 1).leading_zeros()) as usize;
    let dict = fitted(desc, values, true, k, guess(k, bits, nullable))?;
    Ok(if dict.disk_pages() < plain.disk_pages() {
        dict
    } else {
        plain
    })
}

/// Values written plain, each data page within `k` disk pages.
pub fn plain_pages(
    desc: &ColumnDescPtr,
    ty: ColumnType,
    values: &Values,
    k: usize,
) -> Result<Encoded> {
    let nullable = desc.max_def_level() > 0;
    fitted(
        desc,
        values,
        false,
        k,
        guess(k, plain_bits(values, ty), nullable),
    )
}

/// The values of the data pages given, in row order, from their bytes and the dictionary's.
pub fn decode(
    desc: &ColumnDescPtr,
    ty: ColumnType,
    dictionary: Option<&[u8]>,
    pages: &[&[u8]],
    rows: usize,
) -> Result<Values> {
    let mut buf = Vec::new();
    if let Some(d) = dictionary {
        buf.extend_from_slice(d);
    }
    let data_at = buf.len();
    for p in pages {
        buf.extend_from_slice(p);
    }
    let len = buf.len() as i64;
    let meta = ColumnChunkMetaData::builder(desc.clone())
        .set_compression(Compression::UNCOMPRESSED)
        .set_dictionary_page_offset(dictionary.map(|_| 0))
        .set_data_page_offset(data_at as i64)
        .set_total_compressed_size(len)
        .set_total_uncompressed_size(len)
        .set_num_values(rows as i64)
        .build()?;
    let reader: Box<dyn PageReader> = Box::new(SerializedPageReader::new(
        Arc::new(Bytes::from(buf)),
        &meta,
        rows,
        None,
    )?);
    let column = get_column_reader(desc.clone(), reader);
    let mut defs: Vec<i16> = Vec::new();
    let nullable = desc.max_def_level() > 0;
    let short = |n: usize| Error::new(format!("{}: {n} rows decoded of {rows}", desc.name()));
    match ty {
        ColumnType::Int2 | ColumnType::Int4 => {
            let mut r = get_typed_column_reader::<Int32Type>(column);
            let mut v: Vec<i32> = Vec::new();
            let (n, _, _) = r.read_records(rows, nullable.then_some(&mut defs), None, &mut v)?;
            if n != rows {
                return Err(short(n));
            }
            Ok(Values::Int(spread(
                v.into_iter().map(i64::from),
                &defs,
                nullable,
            )))
        }
        ColumnType::Int8 | ColumnType::Timestamptz | ColumnType::Numeric { .. } => {
            let mut r = get_typed_column_reader::<Int64Type>(column);
            let mut v: Vec<i64> = Vec::new();
            let (n, _, _) = r.read_records(rows, nullable.then_some(&mut defs), None, &mut v)?;
            if n != rows {
                return Err(short(n));
            }
            Ok(Values::Int(spread(v.into_iter(), &defs, nullable)))
        }
        ColumnType::Text(_) | ColumnType::Bytes => {
            let mut r = get_typed_column_reader::<ByteArrayType>(column);
            let mut v: Vec<ByteArray> = Vec::new();
            let (n, _, _) = r.read_records(rows, nullable.then_some(&mut defs), None, &mut v)?;
            if n != rows {
                return Err(short(n));
            }
            // Text is UTF-8; bytea's bytes are any.
            let bad = (ty != ColumnType::Bytes)
                .then(|| v.iter().find(|b| std::str::from_utf8(b.data()).is_err()))
                .flatten();
            if let Some(bad) = bad {
                return Err(Error::new(format!(
                    "{}: a value is not UTF-8: {bad:?}",
                    desc.name()
                )));
            }
            Ok(Values::Utf8(spread(v.into_iter(), &defs, nullable)))
        }
    }
}

/// Decodes a block's column as `decode` does, but text read from a dictionary comes back as its
/// codes. The dictionary is decoded once for every run of the block's column that shares it: `dict`
/// holds it from the first run on.
pub fn decode_with(
    desc: &ColumnDescPtr,
    ty: ColumnType,
    dictionary: Option<&[u8]>,
    pages: &[&[u8]],
    rows: usize,
    dict: &mut Option<Arc<[ByteArray]>>,
) -> Result<Values> {
    if let (ColumnType::Text(_), Some(d)) = (ty, dictionary) {
        if let Some(v) = decode_codes(desc, d, pages, rows, dict)? {
            return Ok(v);
        }
    }
    decode(desc, ty, dictionary, pages, rows)
}

/// A dictionary column's codes, page by page, or none where a page is written otherwise.
fn decode_codes(
    desc: &ColumnDescPtr,
    dictionary: &[u8],
    pages: &[&[u8]],
    rows: usize,
    cache: &mut Option<Arc<[ByteArray]>>,
) -> Result<Option<Values>> {
    let mut buf =
        Vec::with_capacity(dictionary.len() + pages.iter().map(|p| p.len()).sum::<usize>());
    buf.extend_from_slice(dictionary);
    let data_at = buf.len();
    for p in pages {
        buf.extend_from_slice(p);
    }
    let len = buf.len() as i64;
    let meta = ColumnChunkMetaData::builder(desc.clone())
        .set_compression(Compression::UNCOMPRESSED)
        .set_dictionary_page_offset(Some(0))
        .set_data_page_offset(data_at as i64)
        .set_total_compressed_size(len)
        .set_total_uncompressed_size(len)
        .set_num_values(rows as i64)
        .build()?;
    let mut reader = SerializedPageReader::new(Arc::new(Bytes::from(buf)), &meta, rows, None)?;
    let bad = |what: String| Error::new(format!("{}: {what}", desc.name()));
    let max_def = desc.max_def_level() as u32;
    let def_width = (32 - max_def.leading_zeros()) as u8;
    let mut codes: Vec<Option<u32>> = Vec::with_capacity(rows);
    let (mut defs, mut present) = (Vec::new(), Vec::new());
    while let Some(page) = reader.get_next_page()? {
        match page {
            Page::DictionaryPage {
                buf,
                num_values,
                encoding,
                ..
            } => {
                if !matches!(encoding, Encoding::PLAIN | Encoding::PLAIN_DICTIONARY) {
                    return Ok(None);
                }
                if cache.is_none() {
                    *cache = Some(plain_text(&buf, num_values as usize).map_err(bad)?);
                }
            }
            Page::DataPage {
                buf,
                num_values,
                encoding,
                def_level_encoding,
                ..
            } => {
                if !matches!(
                    encoding,
                    Encoding::RLE_DICTIONARY | Encoding::PLAIN_DICTIONARY
                ) {
                    return Ok(None);
                }
                let n = num_values as usize;
                let mut at = 0;
                defs.clear();
                if max_def > 0 {
                    if def_level_encoding != Encoding::RLE {
                        return Ok(None);
                    }
                    let l = buf
                        .get(0..4)
                        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize);
                    let levels = l
                        .and_then(|l| buf.get(4..4 + l))
                        .ok_or_else(|| bad("a page ends early".into()))?;
                    hybrid(levels, def_width, n, &mut defs).map_err(bad)?;
                    at = 4 + levels.len();
                }
                let held = if max_def > 0 {
                    defs.iter().filter(|&&d| d == max_def).count()
                } else {
                    n
                };
                let width = *buf.get(at).ok_or_else(|| bad("a page ends early".into()))?;
                present.clear();
                hybrid(&buf[at + 1..], width, held, &mut present).map_err(bad)?;
                let mut p = present.iter();
                if max_def == 0 {
                    codes.extend(p.map(|&c| Some(c)));
                } else {
                    for &d in &defs {
                        codes.push(if d == max_def {
                            p.next().copied()
                        } else {
                            None
                        });
                    }
                }
            }
            _ => return Ok(None),
        }
    }
    let dict = cache
        .clone()
        .ok_or_else(|| bad("its dictionary is missing".into()))?;
    if codes.len() != rows {
        return Err(bad(format!("{} rows decoded of {rows}", codes.len())));
    }
    if codes.iter().flatten().any(|&c| c as usize >= dict.len()) {
        return Err(bad("a code lies past its dictionary".into()));
    }
    Ok(Some(Values::Dict { dict, codes }))
}

/// Text written plain, each value its length in 4 bytes and then its bytes, each checked as UTF-8.
fn plain_text(buf: &Bytes, n: usize) -> std::result::Result<Arc<[ByteArray]>, String> {
    let mut out = Vec::with_capacity(n);
    let mut at = 0;
    for _ in 0..n {
        let ends = || "the dictionary ends early".to_string();
        let l =
            u32::from_le_bytes(buf.get(at..at + 4).ok_or_else(ends)?.try_into().unwrap()) as usize;
        let v = buf.get(at + 4..at + 4 + l).ok_or_else(ends)?;
        std::str::from_utf8(v).map_err(|_| format!("a dictionary entry is not UTF-8: {v:?}"))?;
        out.push(ByteArray::from(buf.slice(at + 4..at + 4 + l)));
        at += 4 + l;
    }
    Ok(out.into())
}

/// `n` values written in runs, each repeated or bit-packed, `width` bits a value, appended to `out`.
fn hybrid(data: &[u8], width: u8, n: usize, out: &mut Vec<u32>) -> std::result::Result<(), String> {
    let short = || "a page's values end early".to_string();
    let want = out.len() + n;
    let w = width as usize;
    let mask = if w >= 32 {
        u32::MAX as u64
    } else {
        (1u64 << w) - 1
    };
    let mut at = 0;
    while out.len() < want {
        let (header, used) = varint(data.get(at..).unwrap_or_default()).ok_or_else(short)?;
        at += used;
        let runs = (header >> 1) as usize;
        if header & 1 == 1 {
            let packed = data.get(at..at + runs * w).ok_or_else(short)?;
            for k in 0..(runs * 8).min(want - out.len()) {
                let bit = k * w;
                let word = match packed.get(bit / 8..bit / 8 + 8) {
                    Some(b) => u64::from_le_bytes(b.try_into().unwrap()),
                    None => packed[bit / 8..]
                        .iter()
                        .rev()
                        .fold(0u64, |v, &b| v << 8 | b as u64),
                };
                out.push(((word >> (bit % 8)) & mask) as u32);
            }
            at += runs * w;
        } else {
            let bytes = data.get(at..at + w.div_ceil(8)).ok_or_else(short)?;
            let value = bytes.iter().rev().fold(0u32, |v, &b| v << 8 | b as u32);
            at += bytes.len();
            out.extend(std::iter::repeat_n(value, runs.min(want - out.len())));
        }
    }
    Ok(())
}

/// An unsigned number written 7 bits a byte, the lowest first; and the bytes it took.
fn varint(data: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &b) in data.iter().enumerate().take(10) {
        v |= ((b & 0x7f) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// The values present, put back among the NULLs their definition levels mark.
fn spread<T>(present: impl Iterator<Item = T>, defs: &[i16], nullable: bool) -> Vec<Option<T>> {
    if !nullable {
        return present.map(Some).collect();
    }
    let mut it = present;
    defs.iter()
        .map(|&d| if d > 0 { it.next() } else { None })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The values decoded again, decoded text as strings, so they compare with the values written.
    #[test]
    fn a_dictionary_column_comes_back_as_its_codes_and_means_the_same_text() {
        let desc = descriptor("feed", ColumnType::Text(None), true).unwrap();
        let cases: Vec<Vec<Option<String>>> = vec![
            vec![Some("AWS Main".to_string()); 5000],
            (0..9000)
                .map(|i| (i % 13 != 0).then(|| format!("feed {}", (i * 7919) % 5)))
                .collect(),
            (0..12000)
                .map(|i| {
                    (i % 101 != 7).then(|| {
                        format!(
                            "station {}",
                            if i % 3 == 0 { 0 } else { (i * 104_729) % 300 }
                        )
                    })
                })
                .collect(),
        ];
        for values in cases {
            let values = Values::Text(values);
            let e = encode(&desc, ColumnType::Text(None), &values, 4).unwrap();
            assert!(e.dictionary.is_some());
            let pages: Vec<&[u8]> = e.pages.iter().map(|p| p.bytes.as_slice()).collect();
            let mut dict = None;
            let got = decode_with(
                &desc,
                ColumnType::Text(None),
                e.dictionary.as_deref(),
                &pages,
                values.len(),
                &mut dict,
            )
            .unwrap();
            assert!(matches!(got, Values::Dict { .. }));
            assert_eq!(
                (0..values.len()).map(|i| got.text(i)).collect::<Vec<_>>(),
                (0..values.len())
                    .map(|i| values.text(i))
                    .collect::<Vec<_>>()
            );
            // A second run of the same block's column takes the dictionary already decoded.
            let held = dict.clone().unwrap();
            let again = decode_with(
                &desc,
                ColumnType::Text(None),
                e.dictionary.as_deref(),
                &pages[..1],
                e.pages[0].rows,
                &mut dict,
            )
            .unwrap();
            assert!(Arc::ptr_eq(again.dict().unwrap(), &held));
        }
    }

    #[test]
    #[allow(clippy::unusual_byte_groupings)]
    fn values_in_runs_repeated_or_bit_packed_come_back_as_written() {
        // A run of 3 sevens, 3 bits a value; then a bit-packed group of 0..8, each byte's digits
        // grouped by the values they hold.
        let mut out = Vec::new();
        hybrid(
            &[
                3 << 1,
                7,
                (1 << 1) | 1,
                0b10_001_000,
                0b1_100_011_0,
                0b111_110_10,
            ],
            3,
            11,
            &mut out,
        )
        .unwrap();
        assert_eq!(out, [7, 7, 7, 0, 1, 2, 3, 4, 5, 6, 7]);
        let mut zeros = Vec::new();
        hybrid(&[(1 << 1) | 1], 0, 5, &mut zeros).unwrap();
        assert_eq!(zeros, [0; 5]);
        assert!(hybrid(&[(1 << 1) | 1, 0], 3, 8, &mut Vec::new()).is_err());
    }

    #[test]
    fn a_dictionary_entry_that_is_not_utf8_is_refused() {
        assert_eq!(
            plain_text(&Bytes::from(vec![1, 0, 0, 0, b'a']), 1).unwrap()[0].data(),
            b"a"
        );
        assert!(plain_text(&Bytes::from(vec![2, 0, 0, 0, 0xff, 0xfe]), 1)
            .unwrap_err()
            .contains("not UTF-8"));
    }

    fn roundtrip(e: &Encoded, desc: &ColumnDescPtr, ty: ColumnType) -> Values {
        let pages: Vec<&[u8]> = e.pages.iter().map(|p| p.bytes.as_slice()).collect();
        let rows = e.pages.iter().map(|p| p.rows).sum();
        match decode(desc, ty, e.dictionary.as_deref(), &pages, rows).unwrap() {
            Values::Utf8(v) => Values::Text(
                v.into_iter()
                    .map(|b| b.map(|b| String::from_utf8(b.data().to_vec()).unwrap()))
                    .collect(),
            ),
            other => other,
        }
    }

    #[test]
    fn distinct_instants_stay_plain_and_fill_their_pages() {
        let desc = descriptor("checked_at", ColumnType::Timestamptz, true).unwrap();
        let values = Values::Int(
            (0..3892)
                .map(|i| {
                    if i % 97 == 0 {
                        None
                    } else {
                        Some(1_669_852_800_000_000 + i * 1_000_003)
                    }
                })
                .collect(),
        );
        let e = encode(&desc, ColumnType::Timestamptz, &values, 8).unwrap();
        assert!(e.dictionary.is_none());
        assert_eq!(e.pages.len(), 1);
        assert_eq!(e.disk_pages(), 8);
        assert_eq!(roundtrip(&e, &desc, ColumnType::Timestamptz), values);
    }

    #[test]
    fn repeated_values_take_a_dictionary_and_each_page_keeps_within_k() {
        let desc = descriptor("station_no", ColumnType::Int4, false).unwrap();
        let values = Values::Int((0..20_000).map(|i| Some(i % 40)).collect());
        let e = encode(&desc, ColumnType::Int4, &values, 1).unwrap();
        assert!(e.dictionary.is_some());
        assert!(e.pages.iter().all(|p| p.bytes.len() <= PAGE));
        assert!(e.disk_pages() < disk_pages(20_000 * 4));
        assert_eq!(e.pages.iter().map(|p| p.rows).sum::<usize>(), 20_000);
        assert_eq!(roundtrip(&e, &desc, ColumnType::Int4), values);
    }

    #[test]
    fn a_guess_too_large_is_cut_down_until_every_page_fits_k() {
        let desc = descriptor("reading_id", ColumnType::Int8, false).unwrap();
        let values = Values::Int((0..5000).map(|i| Some(i * 7919)).collect());
        let e = fitted(&desc, &values, false, 1, 20_000).unwrap();
        assert!(e.pages.len() > 1);
        assert!(e.pages.iter().all(|p| p.bytes.len() <= PAGE));
        assert_eq!(roundtrip(&e, &desc, ColumnType::Int8), values);
    }

    #[test]
    fn a_page_of_k_one_holds_what_one_disk_page_does() {
        let desc = descriptor("reading_id", ColumnType::Int8, false).unwrap();
        let values = Values::Int((0..2000).map(|i| Some(i * 7919)).collect());
        let e = encode(&desc, ColumnType::Int8, &values, 1).unwrap();
        assert!(e.pages.iter().all(|p| p.bytes.len() <= PAGE));
        assert_eq!(e.pages.len(), e.disk_pages());
        assert_eq!(roundtrip(&e, &desc, ColumnType::Int8), values);
    }

    #[test]
    fn five_feeds_take_a_dictionary_of_one_page_beside_one_page_of_indices() {
        let ty = ColumnType::Text(Some(32));
        let desc = descriptor("feed", ty, false).unwrap();
        let feeds = [
            "AWS Backup",
            "AWS Main",
            "AWS hourly",
            "observer",
            "synop relay",
        ];
        let values = Values::Text(
            (0..2142)
                .map(|i| Some(feeds[(i * 7) % 5].to_string()))
                .collect(),
        );
        let e = encode(&desc, ty, &values, 1).unwrap();
        assert!(e.dictionary.is_some());
        assert_eq!(e.disk_pages(), 2);
        assert_eq!(roundtrip(&e, &desc, ty), values);
    }

    #[test]
    fn distinct_text_stays_plain_within_k_and_keeps_its_nulls() {
        let ty = ColumnType::Text(None);
        let desc = descriptor("name", ty, true).unwrap();
        let values = Values::Text(
            (0..3000)
                .map(|i| {
                    if i % 50 == 0 {
                        None
                    } else {
                        Some(format!("Lisboa {}", 10_000 + i))
                    }
                })
                .collect(),
        );
        let e = encode(&desc, ty, &values, 1).unwrap();
        assert!(e.dictionary.is_none());
        assert!(e.pages.iter().all(|p| p.bytes.len() <= PAGE));
        assert!(e.pages.len() > 1);
        assert_eq!(roundtrip(&e, &desc, ty), values);
    }
}
