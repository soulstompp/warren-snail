//! `warren-snail <command> …`: writes a snail from its DDL and Parquet rows, runs SQL statements
//! on it, asks it a question, writes its tables out as Parquet, lists its blocks and its pages, and
//! holds it open. Everything it records is typed Parquet.

use arrow::array::{ArrayRef, BooleanArray, RecordBatch, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema};
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::arrow::ArrowWriter;
use std::fs::File;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use warren_snail::catalog::Catalog;
use warren_snail::cut::PAGE;
#[cfg(feature = "standalone")]
use warren_snail::read::HELD_BYTES;
use warren_snail::read::{Round, Snail};
use warren_snail::write::{parquet_files, sources, write_spanned, Rows};
use warren_snail::{Error, Result};

const USAGE: &str =
    "warren-snail: a database in one file, read a column at a time, declared and asked in
PostgreSQL's SQL. It runs on Linux.

usage: warren-snail write <file> --ddl <ddl.sql> [<table>=<rows.parquet>]... [--file-pages <n>]
                          [--written <written.parquet>]
       warren-snail exec <file> <statements.sql> --out <done.parquet>
       warren-snail read <file> <question.sql> --out <answer.parquet> [--order <name>]
                         [--param <value>]... [--rounds <n>] [--hold <MiB>]
                         [--round <round.parquet>]
       warren-snail out <file> --dir <directory>
       warren-snail blocks <file> --out <blocks.parquet>
       warren-snail census <file>
       warren-snail hold <file> --seconds <n>

  write   a new snail at <file>, where no file may be yet, from the DDL: each table given its rows
          in a Parquet file, or a directory of them as `out` writes them, and a table given none
          written empty. Prints what each order laid out. A row group already cut as the snail
          cuts it comes in by its pages as they lie. The pages lie in files of 1 GiB, <file>, then
          <file>.1 and on; --file-pages sets another size, in 4 KiB pages. --written records each
          order's columns as Parquet.
  exec    each statement in turn, on the snail open to write: INSERT, UPDATE and DELETE; BEGIN,
          COMMIT and ROLLBACK; CREATE TABLE, and CREATE INDEX and ALTER TABLE … CLUSTER ON for a
          table that holds no rows. Outside a transaction each statement commits. --out records
          each statement as Parquet: its rows, its generation, and the pages it read, wrote
          and reused.
  read    the question's answer, as Parquet. Prints each round: the order it read along, and the
          pages it predicted and read. --order names the order to read along; each --param gives
          the next of $1, $2 and on, as text. --rounds asks it again on the one open snail, which
          holds the pages its rounds read, --hold MiB of them (128 unless given). --round records
          each round as Parquet.
  out     each table to <directory>/<table>.parquet, a row group for each block of its order, its
          pages copied as they lie; a block that holds dead rows, or rows written after its sorted
          ones, is pruned on its way out. A table past the size of one of the snail's files goes
          out as a directory of numbered files.
  blocks  one row for each column of each block, as the directory holds it, as Parquet.
  census  every page of the file, by what holds it: named by the newest root, free, or freed once
          an older round ends. Refused where a page is held twice, or by nothing.
  hold    the snail held open for the seconds given, as a round that long would hold it.";

fn save(path: &str, batch: &RecordBatch) -> Result<()> {
    let mut w = ArrowWriter::try_new(
        File::create(path).map_err(|e| Error::new(format!("{path}: {e}")))?,
        batch.schema(),
        None,
    )?;
    w.write(batch)?;
    w.close()?;
    Ok(())
}

fn table(columns: Vec<(&str, ArrayRef)>) -> Result<RecordBatch> {
    let fields: Vec<Field> = columns
        .iter()
        .map(|(n, a)| Field::new(*n, a.data_type().clone(), a.null_count() > 0))
        .collect();
    Ok(RecordBatch::try_new(
        Arc::new(Schema::new(fields)),
        columns.into_iter().map(|(_, a)| a).collect(),
    )?)
}

fn texts(v: Vec<String>) -> ArrayRef {
    Arc::new(StringArray::from(v))
}

fn counts(v: Vec<u64>) -> ArrayRef {
    Arc::new(UInt64Array::from(v))
}

/// This process's bytes read through `read` and `pread`, and the bytes this reading itself took.
fn rchar() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/self/io").unwrap_or_default();
    let n = text
        .lines()
        .find_map(|l| l.strip_prefix("rchar:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0);
    (n, text.len() as u64)
}

/// The bytes the program has written, as the system counts them.
fn wchar() -> u64 {
    std::fs::read_to_string("/proc/self/io")
        .unwrap_or_default()
        .lines()
        .find_map(|l| l.strip_prefix("wchar:"))
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// A count and its noun, the noun plural unless the count is one.
fn n(count: u64, noun: &str) -> String {
    if count == 1 {
        format!("1 {noun}")
    } else {
        format!("{count} {noun}s")
    }
}

fn cmd_census(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let snail = Snail::open(Path::new(file))?;
    let c = snail.census()?;
    println!(
        "generation {}: {} named, {} free, {} waiting to be freed",
        snail.generation(),
        n(c.named, "page"),
        c.free,
        c.waiting
    );
    Ok(())
}

fn cmd_hold(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let seconds: u64 = flag(args, "--seconds")
        .ok_or_else(|| Error::new(USAGE))?
        .parse()
        .map_err(|_| Error::new("--seconds takes a whole number"))?;
    let snail = Snail::open(Path::new(file))?;
    println!("holding generation {}", snail.generation());
    use std::io::Write;
    std::io::stdout().flush()?;
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    drop(snail);
    Ok(())
}

fn cmd_exec(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let sql = args.get(1).ok_or_else(|| Error::new(USAGE))?;
    let out = flag(args, "--out").ok_or_else(|| Error::new(USAGE))?;
    let text = std::fs::read_to_string(sql).map_err(|e| Error::new(format!("{sql}: {e}")))?;
    let statements =
        sqlparser::parser::Parser::parse_sql(&sqlparser::dialect::PostgreSqlDialect {}, &text)?;
    let mut snail = Snail::open_writing(Path::new(file))?;
    let mut done = Vec::with_capacity(statements.len());
    for (i, st) in statements.iter().enumerate() {
        let before = wchar();
        let d = snail
            .execute(&st.to_string())
            .map_err(|e| Error::new(format!("statement {}: {e}", i + 1)))?;
        done.push((d, wchar() - before));
    }
    let system_agrees: Vec<bool> = done
        .iter()
        .map(|(d, bytes)| d.pages_written * PAGE as u64 == *bytes)
        .collect();
    let batch = table(vec![
        ("statement_no", counts((1..=done.len() as u64).collect())),
        (
            "statement",
            texts(done.iter().map(|(d, _)| d.statement.to_string()).collect()),
        ),
        ("rows", counts(done.iter().map(|(d, _)| d.rows).collect())),
        (
            "generation",
            counts(done.iter().map(|(d, _)| d.generation).collect()),
        ),
        (
            "pages_read",
            counts(done.iter().map(|(d, _)| d.pages_read).collect()),
        ),
        (
            "pages_written",
            counts(done.iter().map(|(d, _)| d.pages_written).collect()),
        ),
        (
            "blocks_rewritten",
            counts(done.iter().map(|(d, _)| d.rewritten).collect()),
        ),
        (
            "pages_reused",
            counts(done.iter().map(|(d, _)| d.pages_reused).collect()),
        ),
        (
            "wchar_bytes",
            counts(done.iter().map(|(_, b)| *b).collect()),
        ),
        (
            "system_agrees",
            Arc::new(BooleanArray::from(system_agrees.clone())),
        ),
    ])?;
    save(out, &batch)?;
    let written: u64 = done.iter().map(|(d, _)| d.pages_written).sum();
    let generation = done.last().map_or(0, |(d, _)| d.generation);
    println!(
        "{}, {} written, generation {generation}",
        n(done.len() as u64, "statement"),
        n(written, "page")
    );
    if system_agrees.iter().any(|&k| !k) {
        return Err(Error::new(
            "a statement's pages written differ from the bytes the system counted",
        ));
    }
    Ok(())
}

fn cmd_out(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let dir = flag(args, "--dir").ok_or_else(|| Error::new(USAGE))?;
    std::fs::create_dir_all(dir).map_err(|e| Error::new(format!("{dir}: {e}")))?;
    #[cfg(feature = "standalone")]
    let mut snail = Snail::open_writing(Path::new(file))?;
    #[cfg(not(feature = "standalone"))]
    let mut snail = Snail::open(Path::new(file))?;
    for t in snail.out(Path::new(dir))? {
        println!(
            "{}: {} in {} in {}, {} copied, {} pruned on the way: {}",
            t.table,
            n(t.rows, "row"),
            n(t.row_groups, "row group"),
            n(t.files, "file"),
            n(t.pages, "page"),
            n(t.pruned, "block"),
            t.file.display()
        );
    }
    Ok(())
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}

fn cmd_write(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let ddl_path = flag(args, "--ddl").ok_or_else(|| Error::new(USAGE))?;
    let ddl =
        std::fs::read_to_string(ddl_path).map_err(|e| Error::new(format!("{ddl_path}: {e}")))?;
    let catalog = Catalog::parse(&ddl)?;
    let mut tables = Vec::new();
    let mut given = Vec::new();
    for a in args.iter().skip(1).filter(|a| a.contains('=')) {
        let (name, path) = a.split_once('=').unwrap();
        let t = catalog
            .table(name)
            .ok_or_else(|| Error::new(format!("{name} is not in the DDL")))?;
        // A table past the span comes as a directory of its files, read in turn.
        let files = parquet_files(Path::new(path))?;
        let mut batches = Vec::new();
        for f in &files {
            let file = File::open(f).map_err(|e| Error::new(format!("{}: {e}", f.display())))?;
            batches.extend(
                ParquetRecordBatchReaderBuilder::try_new(file)?
                    .build()?
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        tables.push((name.to_string(), Rows::from_batches(t, &batches)?));
        given.push((name.to_string(), sources(&files, t)?));
    }
    let span = match flag(args, "--file-pages") {
        Some(n) => n
            .parse()
            .map_err(|_| Error::new("--file-pages takes a whole number"))?,
        None => warren_snail::file::FILE_PAGES,
    };
    let written = write_spanned(Path::new(file), &catalog, &tables, &given, span)?;
    if let Some(out) = flag(args, "--written") {
        let rows: Vec<(
            &warren_snail::write::Written,
            &warren_snail::write::ColumnWritten,
        )> = written
            .iter()
            .flat_map(|w| w.columns.iter().map(move |c| (w, c)))
            .collect();
        let own = |w: &warren_snail::write::Written| {
            catalog
                .table(&w.table)
                .and_then(|t| t.orders.iter().find(|o| o.name == w.order))
                .is_some_and(|o| o.table_order)
        };
        let batch = table(vec![
            (
                "table",
                texts(rows.iter().map(|(w, _)| w.table.clone()).collect()),
            ),
            (
                "order",
                texts(rows.iter().map(|(w, _)| w.order.clone()).collect()),
            ),
            (
                "table_order",
                Arc::new(BooleanArray::from(
                    rows.iter().map(|(w, _)| own(w)).collect::<Vec<_>>(),
                )),
            ),
            (
                "rows",
                counts(rows.iter().map(|(w, _)| w.rows as u64).collect()),
            ),
            (
                "blocks",
                counts(rows.iter().map(|(w, _)| w.blocks as u64).collect()),
            ),
            (
                "column",
                texts(rows.iter().map(|(_, c)| c.name.clone()).collect()),
            ),
            ("k", counts(rows.iter().map(|(_, c)| c.k as u64).collect())),
            (
                "data_pages",
                counts(rows.iter().map(|(_, c)| c.data_pages as u64).collect()),
            ),
            (
                "disk_pages",
                counts(rows.iter().map(|(_, c)| c.disk_pages as u64).collect()),
            ),
            (
                "dictionary_blocks",
                counts(
                    rows.iter()
                        .map(|(_, c)| c.dictionary_blocks as u64)
                        .collect(),
                ),
            ),
            (
                "moved_blocks",
                counts(rows.iter().map(|(_, c)| c.moved_blocks as u64).collect()),
            ),
        ])?;
        save(out, &batch)?;
    }
    for w in written {
        println!(
            "{} {}: {}, {}, its directory {} in {}, its page maps {}",
            w.table,
            w.order,
            n(w.rows as u64, "row"),
            n(w.blocks as u64, "block"),
            n(w.index_pages, "page"),
            n(w.levels as u64, "level"),
            n(w.map_pages, "page")
        );
        for c in &w.columns {
            println!(
                "    {:<14} k {:>2}  {:>6} data pages  {:>6} disk pages  dictionary in {} blocks  moved {} of {} blocks{}",
                c.name,
                c.k,
                c.data_pages,
                c.disk_pages,
                c.dictionary_blocks,
                c.moved_blocks,
                w.blocks,
                c.bytes.map_or(String::new(), |b| format!("  {b} bytes"))
            );
        }
    }
    Ok(())
}

fn cmd_read(args: &[String]) -> Result<()> {
    let (file, question) = match args {
        [f, q, ..] => (f, q),
        _ => return Err(Error::new(USAGE)),
    };
    let out = flag(args, "--out").ok_or_else(|| Error::new(USAGE))?;
    let rounds: u64 = match flag(args, "--rounds") {
        Some(n) => n
            .parse()
            .ok()
            .filter(|&n| n > 0)
            .ok_or_else(|| Error::new("--rounds takes a count of rounds, 1 or more"))?,
        None => 1,
    };
    let sql =
        std::fs::read_to_string(question).map_err(|e| Error::new(format!("{question}: {e}")))?;
    let (mut before, mut probe) = rchar();
    let mut began = std::time::Instant::now();
    // Each `--param` in turn gives $1, $2 and on, as text.
    let params: Vec<String> = args
        .windows(2)
        .filter(|w| w[0] == "--param")
        .map(|w| w[1].clone())
        .collect();
    let (mut snail, held_bytes) = open(file, flag(args, "--hold"))?;
    let mut first: Option<RecordBatch> = None;
    let mut done: Vec<Done> = Vec::new();
    for n in 1..=rounds {
        if n > 1 {
            (before, probe) = rchar();
            began = std::time::Instant::now();
        }
        let (read_before, held_before) = if n == 1 {
            (0, 0)
        } else {
            (snail.pages_read(), snail.pages_held())
        };
        let (answer, round) = snail.ask_with(&sql, flag(args, "--order"), &params)?;
        let elapsed_us = began.elapsed().as_micros() as u64;
        let (after, _) = rchar();
        let d = Done {
            n,
            pages_at_open: if n == 1 {
                snail.pages_at_open
            } else {
                round.pages_at_start
            },
            counted: snail.pages_read() - read_before,
            held: snail.pages_held() - held_before,
            bytes: after - before - probe,
            elapsed_us,
            round,
        };
        match &first {
            None => {
                save(out, &answer)?;
                first = Some(answer);
            }
            Some(a) if *a != answer => {
                return Err(Error::new(format!(
                    "round {n} answered otherwise than round 1"
                )))
            }
            Some(_) => {}
        }
        print_round(question, &d);
        done.push(d);
    }
    if let Some(path) = flag(args, "--round") {
        let exact = |e: Option<u64>| e.map_or("-".to_string(), |e| e.to_string());
        let each = |f: &dyn Fn(&Done) -> u64| counts(done.iter().map(f).collect());
        let batch = table(vec![
            (
                "question",
                texts(done.iter().map(|_| question.clone()).collect()),
            ),
            ("round_no", each(&|d| d.n)),
            ("held_bytes", each(&|_| held_bytes)),
            (
                "order",
                texts(done.iter().map(|d| d.round.order.clone()).collect()),
            ),
            (
                "served",
                texts(done.iter().map(|d| d.round.served.join(",")).collect()),
            ),
            (
                "consulted",
                texts(
                    done.iter()
                        .map(|d| {
                            d.round
                                .consulted
                                .iter()
                                .map(|c| {
                                    format!(
                                        "{}:{}:{}-{}:{}:{}",
                                        c.order,
                                        c.directory_pages,
                                        c.low,
                                        c.high,
                                        exact(c.exact),
                                        c.held
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .collect(),
                ),
            ),
            ("rows", each(&|d| d.round.rows)),
            ("pages_at_open", each(&|d| d.pages_at_open)),
            ("directory_pages", each(&|d| d.round.directory_pages)),
            (
                "directory_pages_held",
                each(&|d| d.round.directory_pages_held),
            ),
            (
                "column_pages_top_low",
                each(&|d| d.round.column_pages_top_low),
            ),
            (
                "column_pages_top_high",
                each(&|d| d.round.column_pages_top_high),
            ),
            ("column_pages_low", each(&|d| d.round.column_pages_low)),
            ("column_pages_high", each(&|d| d.round.column_pages_high)),
            (
                "column_pages_predicted",
                each(&|d| d.round.column_pages_predicted),
            ),
            ("column_pages_held", each(&|d| d.round.column_pages_held)),
            ("column_pages_read", each(&|d| d.round.column_pages_read)),
            ("pages_counted", each(&|d| d.counted)),
            ("pages_held", each(&|d| d.held)),
            ("rchar_bytes", each(&|d| d.bytes)),
            (
                "rchar_whole_pages",
                Arc::new(BooleanArray::from(
                    done.iter()
                        .map(|d| d.bytes % PAGE as u64 == 0)
                        .collect::<Vec<_>>(),
                )) as ArrayRef,
            ),
            ("entries", each(&|d| d.round.entries)),
            ("entries_none", each(&|d| d.round.entries_none)),
            ("entries_may", each(&|d| d.round.entries_may)),
            ("entries_must", each(&|d| d.round.entries_must)),
            ("blocks_read", each(&|d| d.round.blocks_read)),
            ("rows_held", each(&|d| d.round.rows_held)),
            ("rows_walked", each(&|d| d.round.rows_walked)),
            ("rows_taken", each(&|d| d.round.rows_taken)),
            ("elapsed_us", each(&|d| d.elapsed_us)),
        ])?;
        save(path, &batch)?;
    }
    Ok(())
}

/// One round of `read`, as it went: the pages read from the file and found held, the bytes the system
/// counted, and the time from opening the file (the first round) or from the round's start.
struct Done {
    n: u64,
    pages_at_open: u64,
    counted: u64,
    held: u64,
    bytes: u64,
    elapsed_us: u64,
    round: Round,
}

/// The snail, holding what `--hold` gives in MiB, or `HELD_BYTES`; and the bytes it holds.
fn open(file: &str, hold: Option<&str>) -> Result<(Snail, u64)> {
    #[cfg(feature = "standalone")]
    {
        let bytes = match hold {
            Some(mib) => {
                mib.parse::<usize>()
                    .map_err(|_| Error::new("--hold takes MiB"))?
                    << 20
            }
            None => HELD_BYTES,
        };
        Ok((Snail::open_holding(Path::new(file), bytes)?, bytes as u64))
    }
    #[cfg(not(feature = "standalone"))]
    {
        if hold.is_some() {
            return Err(Error::new(
                "built without `standalone`, a snail holds no pages of its own: --hold is refused",
            ));
        }
        Ok((Snail::open(Path::new(file))?, 0))
    }
}

fn print_round(question: &str, d: &Done) {
    let r = &d.round;
    println!(
        "{question}, round {}: {}, along {}",
        d.n,
        n(r.rows, "row"),
        r.order
    );
    println!(
        "    pages: {} as it began, {} of the directory ({} held), {} of columns predicted, {} of them \
         held and {} read",
        d.pages_at_open,
        r.directory_pages,
        r.directory_pages_held,
        r.column_pages_predicted,
        r.column_pages_held,
        r.column_pages_read
    );
    println!(
        "    from the file: {}, {} bytes as the system counts them; {} found held",
        n(d.counted, "page"),
        d.bytes,
        n(d.held, "page")
    );
    println!(
        "    the directory's entries: {} looked at, {} holding none of its rows, {} some, {} all; {} \
         read",
        r.entries,
        r.entries_none,
        r.entries_may,
        r.entries_must,
        n(r.blocks_read, "block")
    );
    for c in &r.consulted {
        println!(
            "    {}: {} of its directory; {} to {} column pages by its directory, {}",
            c.order,
            n(c.directory_pages, "page"),
            c.low,
            c.high,
            c.exact.map_or("left unread".to_string(), |e| format!(
                "{e} predicted, {} of them held",
                c.held
            ))
        );
    }
}

fn cmd_blocks(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let out = flag(args, "--out").ok_or_else(|| Error::new(USAGE))?;
    let snail = Snail::open(Path::new(file))?;
    let rows = snail.blocks()?;
    let text =
        |f: &dyn Fn(&warren_snail::read::BlockRow) -> String| texts(rows.iter().map(f).collect());
    let num =
        |f: &dyn Fn(&warren_snail::read::BlockRow) -> u64| counts(rows.iter().map(f).collect());
    let batch = table(vec![
        ("table", text(&|r| r.table.clone())),
        ("order", text(&|r| r.order.clone())),
        ("block", num(&|r| r.block)),
        ("depth", num(&|r| r.depth)),
        ("place", text(&|r| r.place.clone())),
        ("first", text(&|r| r.first.clone())),
        ("last", text(&|r| r.last.clone())),
        ("rows", num(&|r| r.rows)),
        ("map_pages", num(&|r| r.map_pages)),
        ("column", text(&|r| r.column.clone())),
        ("k", num(&|r| r.k)),
        ("dictionary_pages", num(&|r| r.dictionary_pages)),
        ("data_pages", num(&|r| r.data_pages)),
        ("disk_pages", num(&|r| r.disk_pages)),
        ("pages_listed", num(&|r| r.pages_listed)),
        (
            "bytes",
            Arc::new(rows.iter().map(|r| r.bytes).collect::<UInt64Array>()) as ArrayRef,
        ),
        ("nulls", num(&|r| r.nulls)),
        ("dead", num(&|r| r.dead)),
        ("dead_marked", num(&|r| r.dead_marked)),
        ("chains", num(&|r| r.chains)),
        ("files", num(&|r| r.files)),
    ])?;
    save(out, &batch)?;
    println!(
        "{out}: {}, one for each column of each block",
        n(rows.len() as u64, "row")
    );
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("write") => cmd_write(&args[1..]),
        Some("read") => cmd_read(&args[1..]),
        Some("exec") => cmd_exec(&args[1..]),
        Some("out") => cmd_out(&args[1..]),
        Some("hold") => cmd_hold(&args[1..]),
        Some("census") => cmd_census(&args[1..]),
        Some("blocks") => cmd_blocks(&args[1..]),
        Some("help" | "--help" | "-h") => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(Error::new(USAGE)),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            ExitCode::FAILURE
        }
    }
}
