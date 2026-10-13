//! Tables taken out of a snail as Parquet files, a row group a block, and written back in by their
//! pages.
#![cfg(feature = "standalone")]

mod weather;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use warren_snail::catalog::{Catalog, Level};
use warren_snail::read::Snail;
use warren_snail::write::{source, Rows};
use weather::*;

/// A directory of its own for a test's Parquet files.
fn out_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The readings' file: each row group's statistics, and each page's least and greatest in the
/// column index, are those of its rows, for `reading_id` and for `feed`.
fn footer_holds_its_pages(path: &Path) {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::file::metadata::{PageIndexPolicy, ParquetMetaDataReader};
    use parquet::file::page_index::column_index::ColumnIndexMetaData;
    use parquet::file::statistics::Statistics;
    let file = std::fs::File::open(path).unwrap();
    let meta = ParquetMetaDataReader::new()
        .with_page_index_policy(PageIndexPolicy::Required)
        .parse_and_finish(&file)
        .unwrap();
    for (g, rg) in meta.row_groups().iter().enumerate() {
        let batches = ParquetRecordBatchReaderBuilder::try_new(file.try_clone().unwrap())
            .unwrap()
            .with_row_groups(vec![g])
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let ids: Vec<i64> = batches
            .iter()
            .flat_map(|b| ints64(b.column(0).as_ref()))
            .map(Option::unwrap)
            .collect();
        let feeds: Vec<String> = batches
            .iter()
            .flat_map(|b| texts(b.column(3).as_ref()))
            .map(Option::unwrap)
            .collect();
        let Some(Statistics::Int64(s)) = rg.column(0).statistics() else {
            panic!("reading_id has its statistics")
        };
        assert_eq!(
            (s.min_opt(), s.max_opt()),
            (ids.iter().min(), ids.iter().max())
        );
        let s = rg.column(3).statistics().unwrap();
        assert_eq!(s.min_bytes_opt(), feeds.iter().min().map(|x| x.as_bytes()));
        let pages = |c: usize| -> Vec<(usize, usize)> {
            let l = meta.offset_index().unwrap()[g][c].page_locations();
            (0..l.len())
                .map(|p| {
                    let first = l[p].first_row_index as usize;
                    let next = l
                        .get(p + 1)
                        .map_or(ids.len(), |n| n.first_row_index as usize);
                    (first, next)
                })
                .collect()
        };
        let ColumnIndexMetaData::INT64(ix) = &meta.column_index().unwrap()[g][0] else {
            panic!("reading_id has its column index")
        };
        for (p, (a, b)) in pages(0).into_iter().enumerate() {
            assert_eq!(
                (ix.min_value(p), ix.max_value(p)),
                (ids[a..b].iter().min(), ids[a..b].iter().max())
            );
        }
        let ColumnIndexMetaData::BYTE_ARRAY(ix) = &meta.column_index().unwrap()[g][3] else {
            panic!("feed has its column index")
        };
        for (p, (a, b)) in pages(3).into_iter().enumerate() {
            assert_eq!(
                (ix.min_value(p), ix.max_value(p)),
                (
                    feeds[a..b].iter().min().map(|x| x.as_bytes()),
                    feeds[a..b].iter().max().map(|x| x.as_bytes())
                )
            );
        }
    }
}

#[test]
fn a_table_goes_out_as_its_blocks_and_comes_back_in_by_its_pages() {
    let s = sample(20_000);
    let path = snail_spanned("out", &s, warren_snail::file::FILE_PAGES);
    let dir = out_dir("out-parquet");
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let catalog = Catalog::parse(DDL).unwrap();
    let mut tables = Vec::new();
    let mut sources = Vec::new();
    for t in &outs {
        assert_eq!(t.pruned, 0, "a snail written afresh is clean");
        let table = catalog.table(&t.table).unwrap();
        let order = table.orders.iter().find(|o| o.table_order).unwrap();
        let blocks = w
            .blocks()
            .unwrap()
            .iter()
            .filter(|b| b.table == t.table && b.order == order.name)
            .map(|b| b.block)
            .collect::<std::collections::BTreeSet<_>>()
            .len() as u64;
        let (batches, groups) = parquet_of(&t.file);
        assert_eq!((groups as u64, t.row_groups), (blocks, blocks));
        if t.table == "weather.readings" {
            footer_holds_its_pages(&t.file);
        }
        // The file says its order: its DDL declares the table's orders again, and every row group
        // is sorted on the order's levels.
        let meta = parquet::file::reader::FileReader::metadata(
            &parquet::file::reader::SerializedFileReader::new(
                std::fs::File::open(&t.file).unwrap(),
            )
            .unwrap(),
        )
        .clone();
        let ddl = meta
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|kv| kv.key == warren_snail::read::DDL_KEY)
            .and_then(|kv| kv.value.clone())
            .unwrap();
        let said = Catalog::parse(&ddl).unwrap();
        let names = |c: &Catalog| -> Vec<String> {
            c.table(&t.table)
                .unwrap()
                .orders
                .iter()
                .map(|o| o.name.clone())
                .collect()
        };
        assert_eq!(names(&said), names(&catalog));
        let levels: Vec<i32> = order
            .levels
            .iter()
            .map(|l| match l {
                Level::Column(c) => *c as i32,
                Level::MonthUtc(_) => panic!("a table order on a month"),
            })
            .collect();
        for rg in meta.row_groups() {
            let sorting: Vec<i32> = rg
                .sorting_columns()
                .unwrap()
                .iter()
                .map(|s| s.column_idx)
                .collect();
            assert_eq!(sorting, levels);
        }
        let rows = Rows::from_batches(table, &batches).unwrap();
        assert_eq!(rows.len() as u64, t.rows);
        tables.push((t.table.clone(), rows));
        sources.push((t.table.clone(), source(&t.file, table).unwrap()));
    }
    // Back in, every block of each table's order comes by its pages, and every order is cut and
    // laid out as it was.
    let back = tmp("out-back.snail");
    for written in
        warren_snail::write::write_spanned(&back, &catalog, &tables, &sources, span()).unwrap()
    {
        let table = catalog.table(&written.table).unwrap();
        let own = table
            .orders
            .iter()
            .any(|o| o.table_order && o.name == written.order);
        for c in &written.columns {
            if own {
                assert_eq!(c.moved_blocks, written.blocks, "{}", c.name);
            }
        }
    }
    let mut back = Snail::open(&back).unwrap();
    assert_eq!(layout(&back), layout(&w));
    for q in [
        "SELECT count(*) FROM weather.readings r",
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed",
        "SELECT s.name, count(*) FROM weather.stations s GROUP BY s.name",
    ] {
        assert_eq!(
            bag(&back.ask(q, None).unwrap().0),
            bag(&w.ask(q, None).unwrap().0),
            "{q}"
        );
    }
    // A file the plain library writes, its rows in one row group, comes in encoded by the counts.
    let at = outs
        .iter()
        .position(|t| t.table == "weather.readings")
        .unwrap();
    let (batches, _) = parquet_of(&outs[at].file);
    let plain = dir.join("plain.parquet");
    let mut aw = parquet::arrow::ArrowWriter::try_new(
        std::fs::File::create(&plain).unwrap(),
        batches[0].schema(),
        None,
    )
    .unwrap();
    for b in &batches {
        aw.write(b).unwrap();
    }
    aw.close().unwrap();
    sources[at] = (
        outs[at].table.clone(),
        source(&plain, catalog.table(&outs[at].table).unwrap()).unwrap(),
    );
    let again = tmp("out-again.snail");
    for written in
        warren_snail::write::write_spanned(&again, &catalog, &tables, &sources, span()).unwrap()
    {
        if written.table == "weather.readings" {
            assert!(written.columns.iter().all(|c| c.moved_blocks == 0));
        }
    }
    assert_eq!(layout(&Snail::open(&again).unwrap()), layout(&w));
}

#[test]
fn a_block_with_dead_rows_or_rows_out_of_place_is_pruned_on_its_way_out() {
    let s = sample(20_000);
    let path = snail_spanned("out-dirty", &s, warren_snail::file::FILE_PAGES);
    let dir = out_dir("out-dirty-parquet");
    let mut w = Snail::open_writing(&path).unwrap();
    let mut want: BTreeMap<i64, String> = s
        .reading_id
        .iter()
        .zip(&s.feed)
        .map(|(&id, f)| (id, f.clone()))
        .collect();
    // Dead rows at the order's start; at its end, a block whose only fault is a row out of place.
    let order = in_order(&s);
    for i in [10, 500] {
        let id = order[i].2;
        w.execute(&format!(
            "DELETE FROM weather.readings WHERE reading_id = {id}"
        ))
        .unwrap();
        want.remove(&id);
    }
    let id = order[1000].2;
    w.execute(&format!(
        "UPDATE weather.readings SET feed = 'moved on' WHERE reading_id = {id}"
    ))
    .unwrap();
    want.insert(id, "moved on".into());
    let (n, st, _) = order[order.len() - 50];
    w.execute(&format!(
        "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
         VALUES (9000001, {n}, {st}, 'observer', '2022-03-04 05:06:07+00', NULL)"
    ))
    .unwrap();
    want.insert(9_000_001, "observer".into());
    // A snail open only to read cannot prune, so it takes nothing out that is not clean.
    let e = Snail::open(&path)
        .unwrap()
        .out(&dir)
        .unwrap_err()
        .to_string();
    assert!(e.contains("prunes them on their way out"), "{e}");
    let outs = w.out(&dir).unwrap();
    let readings = outs.iter().find(|t| t.table == "weather.readings").unwrap();
    assert!(readings.pruned > 0);
    let (batches, _) = parquet_of(&readings.file);
    let mut got = BTreeMap::new();
    for b in &batches {
        let ids = ints64(b.column_by_name("reading_id").unwrap().as_ref());
        let feeds = texts(b.column_by_name("feed").unwrap().as_ref());
        for (id, f) in ids.into_iter().zip(feeds) {
            got.insert(id.unwrap(), f.unwrap());
        }
    }
    assert_eq!(got, want);
    // Its rows come in the table's order, r_station: (network_id, station_no) never falls.
    let keys: Vec<(i64, i64)> = batches
        .iter()
        .flat_map(|b| {
            ints64(b.column_by_name("network_id").unwrap().as_ref())
                .into_iter()
                .zip(ints64(b.column_by_name("station_no").unwrap().as_ref()))
                .map(|(n, s)| (n.unwrap(), s.unwrap()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(keys.windows(2).all(|k| k[0] <= k[1]));
    // Pruned, the table is clean, and goes out again from a snail open only to read.
    let again = Snail::open(&path).unwrap().out(&dir).unwrap();
    assert!(again.iter().all(|t| t.pruned == 0));
}

/// A table's file written again by the plain library, a row group for each of the file's, in the
/// snail's own Parquet types and in pages of 200 rows: with each row group's rows reversed, or
/// compressed, or each column's dictionary given `dictionary` bytes before its pages go plain.
fn written_again(
    file: &Path,
    to: &Path,
    reversed: bool,
    compression: parquet::basic::Compression,
    dictionary: Option<usize>,
) {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::arrow::arrow_writer::ArrowWriterOptions;
    use parquet::file::properties::{WriterProperties, WriterVersion};
    let builder =
        ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(file).unwrap()).unwrap();
    let groups = builder.metadata().num_row_groups();
    let schema = builder.metadata().file_metadata().schema_descr().clone();
    let arrow_schema = builder.schema().clone();
    let mut props = WriterProperties::builder()
        .set_writer_version(WriterVersion::PARQUET_1_0)
        .set_compression(compression)
        .set_data_page_row_count_limit(200)
        .set_write_batch_size(200);
    if let Some(bytes) = dictionary {
        props = props.set_dictionary_page_size_limit(bytes);
    }
    let props = props.build();
    let mut aw = parquet::arrow::ArrowWriter::try_new_with_options(
        std::fs::File::create(to).unwrap(),
        arrow_schema.clone(),
        ArrowWriterOptions::new()
            .with_properties(props)
            .with_parquet_schema(schema),
    )
    .unwrap();
    for g in 0..groups {
        let batches = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(file).unwrap())
            .unwrap()
            .with_row_groups(vec![g])
            .build()
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        let batch = arrow::compute::concat_batches(&arrow_schema, &batches).unwrap();
        let batch = if reversed {
            let back: arrow::array::UInt32Array = (0..batch.num_rows() as u32).rev().collect();
            arrow::compute::take_record_batch(&batch, &back).unwrap()
        } else {
            batch
        };
        aw.write(&batch).unwrap();
        aw.flush().unwrap();
    }
    aw.close().unwrap();
}

#[test]
fn a_file_the_plain_library_writes_comes_in_by_its_pages_only_where_the_snail_holds_them() {
    use parquet::basic::Compression;
    let s = sample(3_000);
    let path = snail_spanned("out-plain", &s, warren_snail::file::FILE_PAGES);
    let dir = out_dir("out-plain-parquet");
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let catalog = Catalog::parse(DDL).unwrap();
    let file = outs
        .iter()
        .find(|t| t.table == "weather.readings")
        .unwrap()
        .file
        .clone();
    let (n, st) = (s.network_id[1500], s.station_no[1500]);
    let questions = [
        format!(
            "SELECT r.reading_id, r.feed FROM weather.readings r \
             WHERE r.network_id = {n} AND r.station_no = {st}"
        ),
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed".to_string(),
    ];
    // Each way the readings are written again: whether their pages may come in as they lie.
    for (name, reversed, compression, dictionary, moves) in [
        ("in-order", false, Compression::UNCOMPRESSED, None, true),
        ("reversed", true, Compression::UNCOMPRESSED, None, false),
        ("compressed", false, Compression::SNAPPY, None, false),
        (
            "dictionary-spent",
            false,
            Compression::UNCOMPRESSED,
            Some(16),
            false,
        ),
    ] {
        let again = dir.join(format!("{name}.parquet"));
        written_again(&file, &again, reversed, compression, dictionary);
        let mut tables = Vec::new();
        let mut sources = Vec::new();
        for t in &outs {
            let table = catalog.table(&t.table).unwrap();
            let f = if t.table == "weather.readings" {
                again.clone()
            } else {
                t.file.clone()
            };
            let (batches, _) = parquet_of(&f);
            tables.push((
                t.table.clone(),
                Rows::from_batches(table, &batches).unwrap(),
            ));
            sources.push((t.table.clone(), source(&f, table).unwrap()));
        }
        // The same rows written afresh: where the counts choose a dictionary.
        let afresh = warren_snail::write::write_spanned(
            &tmp(&format!("out-plain-{name}-afresh.snail")),
            &catalog,
            &tables,
            &[],
            span(),
        )
        .unwrap();
        let chosen = |column: &str| -> usize {
            afresh
                .iter()
                .find(|w| w.order == "r_station")
                .unwrap()
                .columns
                .iter()
                .find(|c| c.name == column)
                .unwrap()
                .dictionary_blocks
        };
        let snail_again = tmp(&format!("out-plain-{name}.snail"));
        for written in
            warren_snail::write::write_spanned(&snail_again, &catalog, &tables, &sources, span())
                .unwrap()
        {
            if written.table == "weather.readings" && written.order == "r_station" {
                // A column moves where the file holds it as the counts would, on a dictionary in
                // every page or on none: the file's columns all take a dictionary, so each moves
                // where the counts choose one; where its dictionary ran out, its later pages plain,
                // it never does.
                for c in &written.columns {
                    let want = if moves { chosen(&c.name) } else { 0 };
                    assert_eq!(c.moved_blocks, want, "{name} {}", c.name);
                }
                let moved: usize = written.columns.iter().map(|c| c.moved_blocks).sum();
                assert_eq!(moved > 0, moves, "{name}");
            }
        }
        let mut snail_again = Snail::open(&snail_again).unwrap();
        // Pages that came as they lay are the file's, in pages of 200 rows, not the snail's own.
        assert_eq!(layout(&snail_again) == layout(&w), !moves, "{name}");
        for q in &questions {
            assert_eq!(
                bag(&snail_again.ask(q, None).unwrap().0),
                bag(&w.ask(q, None).unwrap().0),
                "{name}: {q}"
            );
        }
    }
}

#[test]
fn a_table_past_the_span_goes_out_as_files_of_whole_row_groups_and_comes_back_by_its_pages() {
    use warren_snail::write::{parquet_files, sources};
    let s = sample(20_000);
    let path = snail_spanned("out-split", &s, 64);
    let dir = out_dir("out-split-parquet");
    let mut w = Snail::open_writing(&path).unwrap();
    let outs = w.out(&dir).unwrap();
    let catalog = Catalog::parse(DDL).unwrap();
    let mut tables = Vec::new();
    let mut given = Vec::new();
    for t in &outs {
        let table = catalog.table(&t.table).unwrap();
        let files = parquet_files(&t.file).unwrap();
        assert_eq!(files.len() as u64, t.files);
        let mut batches = Vec::new();
        let mut groups = 0;
        for f in &files {
            let (b, g) = parquet_of(f);
            batches.extend(b);
            groups += g;
            // Each file holds whole row groups, within the span unless one row group passes it.
            if g > 1 {
                assert!(std::fs::metadata(f).unwrap().len() <= 64 * 4096 + 64 * 1024);
            }
        }
        assert_eq!(groups as u64, t.row_groups);
        tables.push((
            t.table.clone(),
            Rows::from_batches(table, &batches).unwrap(),
        ));
        given.push((t.table.clone(), sources(&files, table).unwrap()));
    }
    let readings = outs.iter().find(|t| t.table == "weather.readings").unwrap();
    assert!(readings.files > 1, "{} files", readings.files);
    let back = tmp("out-split-back.snail");
    for written in warren_snail::write::write_spanned(&back, &catalog, &tables, &given, 64).unwrap()
    {
        if written.order == "r_station" {
            assert!(written
                .columns
                .iter()
                .all(|c| c.moved_blocks == written.blocks));
        }
    }
    assert_eq!(layout(&Snail::open(&back).unwrap()), layout(&w));
}
