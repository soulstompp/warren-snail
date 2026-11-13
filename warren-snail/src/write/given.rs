//! Where a table's rows come from as it is written, a chunk at a time: rows held whole, a table's
//! Parquet files read a batch at a time, or an index's element strand made from its table's rows as
//! they come; and the checks a table's rows meet before any of its blocks is written.

use super::Rows;
use crate::catalog::{Catalog, ColumnType, Table};
use crate::values::Values;
use crate::{Error, Result};
use std::path::PathBuf;

/// The rows held whole a chunk hands on at most.
const HELD_ROWS: usize = 4096;

/// A table's rows, as the write is given them.
pub(crate) enum Given<'g> {
    /// Rows held whole by the caller.
    Held(&'g Rows),
    /// A table's Parquet files, read a batch at a time as `table` reads them.
    Files { table: Table, files: Vec<PathBuf> },
    /// The element strand `strand` of the table `of` names, made from that table's rows.
    Strand {
        of: String,
        rows: &'g Given<'g>,
        strand: String,
    },
}

impl Given<'_> {
    /// Hands `each` the rows a chunk at a time, in their order.
    pub fn each(&self, catalog: &Catalog, each: &mut dyn FnMut(Rows) -> Result<()>) -> Result<()> {
        match self {
            Given::Held(rows) => {
                let n = rows.len();
                let mut a = 0;
                while a < n {
                    let b = (a + HELD_ROWS).min(n);
                    let at: Vec<usize> = (a..b).collect();
                    each(Rows {
                        columns: rows.columns.iter().map(|v| v.gather(&at)).collect(),
                    })?;
                    a = b;
                }
                Ok(())
            }
            Given::Files { table, files } => {
                use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
                for f in files {
                    let file = std::fs::File::open(f)
                        .map_err(|e| Error::new(format!("{}: {e}", f.display())))?;
                    for b in ParquetRecordBatchReaderBuilder::try_new(file)?.build()? {
                        each(Rows::from_batches(table, &[b?])?)?;
                    }
                }
                Ok(())
            }
            Given::Strand { of, rows, strand } => rows.each(catalog, &mut |chunk| {
                let made = crate::read::elements::strands_of(catalog, &[(of.clone(), chunk)])?;
                match made.into_iter().find(|(n, _)| n == strand) {
                    Some((_, r)) => each(r),
                    None => Ok(()),
                }
            }),
        }
    }

    /// Whether every value of a table's file names a column of the right type: refused as a
    /// batch of them would be, before any row is read.
    pub fn schema_checked(&self) -> Result<()> {
        let Given::Files { table, files } = self else {
            return Ok(());
        };
        use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
        for f in files {
            let file =
                std::fs::File::open(f).map_err(|e| Error::new(format!("{}: {e}", f.display())))?;
            let schema = ParquetRecordBatchReaderBuilder::try_new(file)?
                .schema()
                .clone();
            Rows::from_batches(table, &[arrow::array::RecordBatch::new_empty(schema)])?;
        }
        Ok(())
    }

    /// The rows whole, as a write in an older format reads them.
    pub fn whole(&self, catalog: &Catalog) -> Result<Rows> {
        if let Given::Held(rows) = self {
            return Ok(Rows {
                columns: rows.columns.clone(),
            });
        }
        if let Given::Files { table, files } = self {
            use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
            let mut batches = Vec::new();
            for f in files {
                let file = std::fs::File::open(f)
                    .map_err(|e| Error::new(format!("{}: {e}", f.display())))?;
                batches.extend(
                    ParquetRecordBatchReaderBuilder::try_new(file)?
                        .build()?
                        .collect::<std::result::Result<Vec<_>, _>>()?,
                );
            }
            return Rows::from_batches(table, &batches);
        }
        let mut out: Option<Rows> = None;
        self.each(catalog, &mut |chunk| {
            appended(&mut out, &chunk);
            Ok(())
        })?;
        Ok(out.unwrap_or(Rows {
            columns: Vec::new(),
        }))
    }
}

/// `chunk`'s rows after those of `out`.
pub(crate) fn appended(out: &mut Option<Rows>, chunk: &Rows) {
    let out = out.get_or_insert_with(|| Rows {
        columns: chunk.columns.iter().map(Values::new_like).collect(),
    });
    for (o, v) in out.columns.iter_mut().zip(&chunk.columns) {
        for i in 0..v.len() {
            o.push_from(v, i);
        }
    }
}

/// What a table's rows must meet before any of its blocks is written, met a chunk at a time and
/// refused as a write of every row at once refuses them: each column in turn, its length, then a
/// varchar's longest value; then the first row its domains, NOT NULLs and CHECKs refuse; then an
/// EXCLUDE.
pub(crate) struct Checks<'c> {
    catalog: &'c Catalog,
    table: &'c Table,
    short: Vec<bool>,
    longest: Vec<usize>,
    /// Whether every row is met, or only one holding a NULL in a NOT NULL column.
    every: bool,
    nulls: bool,
    first: Option<Error>,
    rows: usize,
}

impl<'c> Checks<'c> {
    pub fn new(catalog: &'c Catalog, table: &'c Table) -> Checks<'c> {
        let composed = table
            .columns
            .iter()
            .any(|c| matches!(Values::new(c.ty), Values::Composed(_)));
        Checks {
            catalog,
            table,
            short: vec![false; table.columns.len()],
            longest: vec![0; table.columns.len()],
            every: table.checks.iter().any(|k| k.enforced)
                || table.columns.iter().any(|c| c.domain.is_some())
                || composed,
            nulls: false,
            first: None,
            rows: 0,
        }
    }

    /// Each column's length against the rows', and a varchar's longest value.
    pub fn columns(&mut self, rows: &Rows) {
        for (ci, (c, values)) in self.table.columns.iter().zip(&rows.columns).enumerate() {
            if values.len() != rows.len() {
                self.short[ci] = true;
                continue;
            }
            if let ColumnType::Text(Some(_)) = c.ty {
                self.longest[ci] = self.longest[ci].max(values.longest());
            }
        }
    }

    /// Whether a column's length differs from the rows'.
    pub fn short(&self) -> bool {
        self.short.contains(&true)
    }

    /// Each row in turn against its columns' domains, then the table's NOT NULLs and CHECKs, as
    /// PostgreSQL's COPY checks them.
    pub fn rows(&mut self, rows: &Rows) -> Result<()> {
        let table = self.table;
        self.rows += rows.len();
        let not_null: Vec<usize> = (0..table.columns.len().min(rows.columns.len()))
            .filter(|&c| table.columns[c].not_null && rows.columns[c].nulls() > 0)
            .collect();
        self.nulls |= !not_null.is_empty();
        if self.first.is_some() || (!self.every && not_null.is_empty()) {
            return Ok(());
        }
        let _own = crate::read::own::holding(&self.catalog.functions, self.catalog.release);
        for i in 0..rows.len() {
            if !self.every && !not_null.iter().any(|&c| rows.columns[c].is_null(i)) {
                continue;
            }
            let row: crate::read::statement::Row = table
                .columns
                .iter()
                .zip(&rows.columns)
                .map(|(c, v)| super::row_value(c.ty, v, i))
                .collect();
            let met = crate::read::fill::check_domains(&self.catalog.domains, table, &row)
                .and_then(|_| crate::read::fill::check(table, &row));
            if let Err(e) = met {
                self.first = Some(e);
                break;
            }
        }
        Ok(())
    }

    /// The first refusal, in the order a write of every row at once meets them.
    pub fn finish(self) -> Result<()> {
        let table = self.table;
        for (ci, c) in table.columns.iter().enumerate() {
            if self.short[ci] {
                return Err(Error::new(format!(
                    "{}: its columns differ in length",
                    table.name
                )));
            }
            if let ColumnType::Text(Some(n)) = c.ty {
                if self.longest[ci] > n as usize {
                    return Err(Error::new(format!(
                        "{}.{} is varchar({n}) and a row holds {} characters",
                        table.name, c.name, self.longest[ci]
                    )));
                }
            }
        }
        let met = self.nulls
            || table.checks.iter().any(|k| k.enforced)
            || table.columns.iter().any(|c| c.domain.is_some());
        if let Some(e) = self.first.filter(|_| met) {
            return Err(e);
        }
        if let Some(o) = table.orders.iter().find(|o| o.exclusion.is_some()) {
            if self.rows > 0 {
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
        Ok(())
    }
}
