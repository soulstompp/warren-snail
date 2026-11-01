//! `warren-snail <command> …`: writes a snail from its DDL and Parquet rows, runs SQL statements
//! on it, asks it a question, writes its tables out as Parquet, lists its blocks and its pages,
//! writes it afresh under its collations' versions, reads a DDL as the snail reads it, lists the
//! functions PostgreSQL defines, and holds it open. Everything it records is typed Parquet.

use arrow::array::{ArrayRef, BooleanArray, RecordBatch, StringArray, UInt64Array};
use arrow::datatypes::{Field, Schema};
use parquet::arrow::ArrowWriter;
use std::fs::File;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use warren_snail::catalog::Catalog;
use warren_snail::cut::PAGE;
#[cfg(feature = "standalone")]
use warren_snail::read::HELD_BYTES;
use warren_snail::read::{Round, Snail, Snails};
use warren_snail::write::write_parquet;
use warren_snail::{Error, Result};

const USAGE: &str =
    "warren-snail: a database in one file, read a column at a time, declared and asked in
PostgreSQL's SQL. It runs on Linux.

usage: warren-snail write <file> --ddl <ddl.sql> [<table>=<rows.parquet>]... [--file-pages <n>]
                          [--release <14..19>] [--lc-monetary <locale>]
                          [--written <written.parquet>]
       warren-snail exec <file> <statements.sql> --out <done.parquet> [--with <file>]...
                         [--host <command> --host-server <name>]
       warren-snail read <file> <question.sql> --out <answer.parquet> [--order <name>]
                         [--param <value>]... [--rounds <n>] [--hold <MiB>]
                         [--round <round.parquet>] [--text] [--with <file>]...
       warren-snail out <file> --dir <directory>
       warren-snail blocks <file> --out <blocks.parquet>
       warren-snail census <file>
       warren-snail recut <file> --to <new file>
       warren-snail catalog (<file> | --ddl <ddl.sql> [--taken <taken.sql>]
                            [--statements <statements.parquet>]) [--out <catalog.parquet>]
       warren-snail functions [--release <14..19>] [--out <functions.parquet>]
       warren-snail hold <file> --seconds <n>

  write   a new snail at <file>, where no file may be yet, from the DDL: each table given its rows
          in a Parquet file, or a directory of them as `out` writes them, and a table given none
          written empty. Prints what each order laid out. A row group already cut as the snail
          cuts it comes in by its pages as they lie. The pages lie in files of 1 GiB, <file>, then
          <file>.1 and on; --file-pages sets another size, in 4 KiB pages. The release the rows'
          OIDs came from, whose built-in objects name its reg* values, and the lc_monetary their
          money was written under, which the snail reads and prints its money under, are those
          the files name, as `out` names them; where they name none, --release and --lc-monetary
          give them, 19 and C unless given, any locale but C and POSIX through glibc, with the
          feature collations. A flag naming another than the files name is refused. --written
          records each order's columns as Parquet.
  exec    each statement in turn, on the snail open to write: INSERT, UPDATE and DELETE; BEGIN,
          COMMIT, ROLLBACK and SET CONSTRAINTS; and the DDL `write` reads, a table's columns and
          indexes changed only while it holds no rows. Outside a transaction each statement
          commits. --out records each statement as Parquet: its rows, its generation, and the
          pages it read, wrote and reused. Each --with opens one more snail with <file>, as one
          writer of them all: a statement writes the snail that holds its table, and a
          transaction's COMMIT commits every snail it wrote, one root deciding.
          A snail PostgreSQL writes through its WAL is written only through PostgreSQL, its host:
          --host names the command that carries each commit to it, psql for one, read a
          statement at a time; the statements run as one transaction of the host's, under the
          writers' lock PostgreSQL's writers take, the server warren_snail_pg names the snail
          by --host-server handing it the root it takes, logging the commit and writing its
          root.
  read    the question's answer, as Parquet. Prints each round: the order it read along, and the
          pages it predicted and read. --order names the order to read along; each --param gives
          the next of $1, $2 and on, as text. --rounds asks it again on the one open snail, which
          holds the pages its rounds read, --hold MiB of them (128 unless given). --round records
          each round as Parquet. --text writes each value of a time type, a scalar's and an
          interval's, as PostgreSQL's output writes it, text exactly as the value. Each --with
          opens one more snail with <file>, each holding --hold MiB: every round reads one moment
          of them all, and a join of their tables reads each table from its own snail.
  out     each table to <directory>/<table>.parquet, a row group for each block of its order, its
          pages copied as they lie; a block that holds dead rows, or rows written after its sorted
          ones, is pruned on its way out. A table past the size of one of the snail's files goes
          out as a directory of numbered files.
  blocks  one row for each column of each block, as the directory holds it, as Parquet.
  census  every page of the file, by what holds it: named by the newest root, free, or freed once
          an older round ends. Refused where a page is held twice, or by nothing.
  recut   the snail written afresh at <new file>, where no file may be yet, every order cut under
          the collations' versions this build reads: each table out, then written back, carrying
          where its sequences stand. An order cut under another version is refused until re-cut.
  catalog what the snail reads of a DDL, each statement taken or refused by name, and what it
          reads and does nothing with; or the catalog a snail holds. --taken writes the
          statements taken, as a DDL a snail can be written from. --statements records each
          statement as Parquet, and --out each fact of the catalog: a row each, its kind, its
          object, the fact and its value.
  functions every function PostgreSQL defines, 19 unless --release names another release: a type's
          the snail answers, a type's it does not answer, or the server's own, which it refuses
          by name. Prints how many of each; --out records each as Parquet: its signature, class,
          family and whether the snail answers it.
  hold    the snail held open for the seconds given, as a round that long would hold it.

  Built with the feature zones, SNAIL_ZONES names the directory compiled zones are read from: the
  machine's /usr/share/zoneinfo unless given.";

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

/// An answer with each column of a time type, a scalar or an interval written as text, as
/// PostgreSQL's output writes each value.
fn as_text(batch: &RecordBatch) -> Result<RecordBatch> {
    use arrow::array::{Array, StringArray};
    use arrow::datatypes::DataType as A;
    use warren_snail::catalog::ColumnType;
    let schema = batch.schema();
    let mut fields = Vec::new();
    let mut columns: Vec<ArrayRef> = Vec::new();
    for (a, f) in batch.columns().iter().zip(schema.fields()) {
        let ty = warren_snail::scalars::field_scalar(f).or(match f.data_type() {
            A::Timestamp(_, Some(_)) => Some(ColumnType::Timestamptz),
            _ => None,
        });
        let Some(ty) = ty else {
            fields.push(f.as_ref().clone());
            columns.push(a.clone());
            continue;
        };
        let text: StringArray = (0..a.len())
            .map(|r| match ty {
                _ if a.is_null(r) => None,
                ColumnType::Interval { .. } => warren_snail::scalars::interval_at(a.as_ref(), r)
                    .map(warren_snail::times::interval_out),
                ty => warren_snail::scalars::scalar_key_at(ty, a.as_ref(), r)
                    .map(|k| warren_snail::scalars::scalar_text(ty, k)),
            })
            .collect();
        fields.push(Field::new(f.name(), A::Utf8, f.is_nullable()));
        columns.push(Arc::new(text));
    }
    Ok(RecordBatch::try_new(
        Arc::new(arrow::datatypes::Schema::new(fields)),
        columns,
    )?)
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

/// The line the host's command prints after each statement it is handed has run.
const MARK: &str = "--warren-snail-ran--";

/// The command that carries each commit of a snail written through its host: a process reading
/// statements as they come, PostgreSQL's psql for one, its output read back up to a mark after each.
struct Carrier {
    child: std::process::Child,
    stdin: Option<std::process::ChildStdin>,
    stdout: std::io::BufReader<std::process::ChildStdout>,
    /// The bytes handed it, which the command writes beside the snail's pages.
    written: u64,
}

impl Carrier {
    fn start(command: &str) -> Result<Carrier> {
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| Error::new(format!("{command}: {e}")))?;
        let stdin = child.stdin.take();
        let stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        Ok(Carrier {
            child,
            stdin,
            stdout,
            written: 0,
        })
    }

    /// One statement run by the host, and the lines it printed; refused where the command ends
    /// before the statement has run, as one that fails does.
    fn run(&mut self, sql: &str) -> Result<Vec<String>> {
        use std::io::{BufRead, Write};
        let text = format!("{sql};\n\\echo {MARK}\n");
        let ended = |e: std::io::Error| {
            let shown: String = sql.chars().take(200).collect();
            Error::new(format!("the host ended before it ran {shown}: {e}"))
        };
        let stdin = self.stdin.as_mut().expect("the host's input is open");
        stdin
            .write_all(text.as_bytes())
            .and_then(|_| stdin.flush())
            .map_err(ended)?;
        self.written += text.len() as u64;
        let mut out = Vec::new();
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).map_err(ended)? == 0 {
                return Err(ended(std::io::Error::other("its output ended")));
            }
            let line = line.trim_end_matches('\n');
            if line == MARK {
                return Ok(out);
            }
            out.push(line.to_string());
        }
    }

    /// The last line a statement printed.
    fn one(&mut self, sql: &str) -> Result<String> {
        Ok(self.run(sql)?.pop().unwrap_or_default())
    }

    /// The host's input closed, and the command ended, refused where it failed.
    fn finish(mut self) -> Result<()> {
        drop(self.stdin.take());
        let status = self.child.wait()?;
        if !status.success() {
            return Err(Error::new(format!(
                "the host's command ended with {status}"
            )));
        }
        Ok(())
    }
}

impl Drop for Carrier {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

/// A literal of the host's SQL.
fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The host that carries each commit to the server and writes its root there: logged in the
/// server's WAL, the root tagged with the server's transaction.
struct Hosted {
    carrier: Arc<std::sync::Mutex<Carrier>>,
    server: String,
    tag: u64,
}

impl warren_snail::carry::Host for Hosted {
    fn carry(&mut self, commit: &warren_snail::carry::Commit) -> Result<()> {
        let bytes = commit.encode();
        let mut hex = String::with_capacity(bytes.len() * 2);
        for b in bytes {
            hex.push_str(&format!("{b:02x}"));
        }
        let sql = format!(
            "SELECT warren_snail_commit({}, decode('{hex}', 'hex'))",
            literal(&self.server)
        );
        self.carrier.lock().unwrap().run(&sql).map(|_| ())
    }

    fn tag(&self) -> Option<u64> {
        Some(self.tag)
    }

    fn keeps_root(&self) -> bool {
        true
    }
}

/// The judge the server is: it hands the writer the root it takes, the writer holding the writers'
/// lock, and says no word on a copy.
struct Asked {
    carrier: Arc<std::sync::Mutex<Carrier>>,
    server: String,
}

impl warren_snail::carry::Judge for Asked {
    fn taken(&self, _tag: u64) -> Result<bool> {
        Err(Error::new(
            "the host hands the root it takes, and says no word on a copy",
        ))
    }

    fn root(&self) -> Result<Option<Vec<u8>>> {
        let hex = self.carrier.lock().unwrap().one(&format!(
            "SELECT encode(warren_snail_root({}), 'hex')",
            literal(&self.server)
        ))?;
        let bytes: Option<Vec<u8>> = (0..hex.len())
            .step_by(2)
            .map(|i| {
                hex.get(i..i + 2)
                    .and_then(|b| u8::from_str_radix(b, 16).ok())
            })
            .collect();
        bytes
            .filter(|b| !b.is_empty())
            .map(Some)
            .ok_or_else(|| Error::new("the host handed no root"))
    }
}

/// The statements run as one transaction of the host's, on a snail its host writes: the host's
/// command takes the writers' lock in a transaction of its own and hands the snail the root it
/// takes, the snail choosing its pages from that root's free map, and its one commit is handed to
/// it, the host writing the root; then the host's transaction commits. Each statement's account, the
/// commit's last.
fn exec_hosted(
    file: &str,
    statements: &[String],
    command: &str,
    server: &str,
) -> Result<Vec<(warren_snail::read::Done, u64)>> {
    for (i, st) in statements.iter().enumerate() {
        let word = st
            .trim_start()
            .split(|c: char| !c.is_ascii_alphabetic())
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if [
            "BEGIN",
            "START",
            "COMMIT",
            "END",
            "ROLLBACK",
            "ABORT",
            "SAVEPOINT",
            "RELEASE",
        ]
        .contains(&word.as_str())
        {
            return Err(Error::new(format!(
                "statement {}: under --host the statements run as one transaction of the host's: \
                 {word} is refused",
                i + 1
            )));
        }
    }
    let path = std::fs::canonicalize(file).map_err(|e| Error::new(format!("{file}: {e}")))?;
    let carrier = Arc::new(std::sync::Mutex::new(Carrier::start(command)?));
    carrier.lock().unwrap().run("BEGIN")?;
    let tag: u64 = carrier
        .lock()
        .unwrap()
        .one(&format!(
            "SELECT warren_snail_writer({}, {})",
            literal(server),
            literal(&path.to_string_lossy())
        ))?
        .parse()
        .map_err(|_| Error::new("the host gave no transaction's tag"))?;
    let host = Hosted {
        carrier: carrier.clone(),
        server: server.to_string(),
        tag,
    };
    let judge = std::rc::Rc::new(Asked {
        carrier: carrier.clone(),
        server: server.to_string(),
    });
    let mut snail = Snail::open_carried(Path::new(file), Box::new(host), judge)?;
    let handed = || carrier.lock().unwrap().written;
    let mut done = Vec::with_capacity(statements.len() + 1);
    snail.execute("BEGIN")?;
    let all = statements.iter().map(String::as_str).chain(["COMMIT"]);
    for (i, st) in all.enumerate() {
        let (before, carried) = (wchar(), handed());
        let d = snail
            .execute(st)
            .map_err(|e| Error::new(format!("statement {}: {e}", i + 1)))?;
        done.push((d, wchar() - before - (handed() - carried)));
    }
    drop(snail);
    carrier.lock().unwrap().run("COMMIT")?;
    let carrier = Arc::try_unwrap(carrier)
        .ok()
        .expect("the snail let go of its host")
        .into_inner()
        .unwrap();
    carrier.finish()?;
    Ok(done)
}

fn cmd_exec(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let sql = args.get(1).ok_or_else(|| Error::new(USAGE))?;
    let out = flag(args, "--out").ok_or_else(|| Error::new(USAGE))?;
    let text = std::fs::read_to_string(sql).map_err(|e| Error::new(format!("{sql}: {e}")))?;
    // Each statement as written: the snail reads itself what sqlparser does not.
    let statements = warren_snail::read::statement::split(&text)?;
    let others = withs(args);
    let done = match flag(args, "--host") {
        Some(command) => {
            let server = flag(args, "--host-server").ok_or_else(|| {
                Error::new("--host takes --host-server, the host's name for the snail")
            })?;
            if !others.is_empty() {
                return Err(Error::new("--host writes one snail: --with is refused"));
            }
            exec_hosted(file, &statements, command, server)?
        }
        None => {
            let mut opened = if others.is_empty() {
                Opened::One(Snail::open_writing(Path::new(file))?)
            } else {
                let paths: Vec<&Path> = std::iter::once(file.as_str())
                    .chain(others)
                    .map(Path::new)
                    .collect();
                Opened::Several(Snails::open_writing(&paths)?)
            };
            let mut done = Vec::with_capacity(statements.len());
            for (i, st) in statements.iter().enumerate() {
                let before = wchar();
                let d = match &mut opened {
                    Opened::One(s) => s.execute(st),
                    Opened::Several(s) => s.execute(st),
                }
                .map_err(|e| Error::new(format!("statement {}: {e}", i + 1)))?;
                done.push((d, wchar() - before));
            }
            done
        }
    };
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
        if !t.extremes.is_empty() {
            println!(
                "{}: {} hold an infinity or 24:00:00, written as the extremes of their page forms",
                t.table,
                t.extremes.join(", ")
            );
        }
    }
    Ok(())
}

fn cmd_recut(args: &[String]) -> Result<()> {
    let file = args.first().ok_or_else(|| Error::new(USAGE))?;
    let to = flag(args, "--to").ok_or_else(|| Error::new(USAGE))?;
    #[cfg(feature = "standalone")]
    let mut snail = Snail::open_writing(Path::new(file))?;
    #[cfg(not(feature = "standalone"))]
    let mut snail = Snail::open(Path::new(file))?;
    for w in snail.recut(Path::new(to))? {
        println!(
            "{} {}: {}, {}",
            w.table,
            w.order,
            n(w.rows as u64, "row"),
            n(w.blocks as u64, "block")
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
    // A table past the span comes as a directory of its files, read in turn.
    let given: Vec<(String, std::path::PathBuf)> = args
        .iter()
        .skip(1)
        .filter(|a| a.contains('='))
        .map(|a| {
            let (name, path) = a.split_once('=').unwrap();
            (name.to_string(), path.into())
        })
        .collect();
    let span = match flag(args, "--file-pages") {
        Some(n) => n
            .parse()
            .map_err(|_| Error::new("--file-pages takes a whole number"))?,
        None => warren_snail::file::FILE_PAGES,
    };
    let release = flag(args, "--release")
        .map(|r| {
            r.parse()
                .map_err(|_| Error::new("--release takes PostgreSQL's major release, 14 to 19"))
        })
        .transpose()?;
    let written = write_parquet(
        Path::new(file),
        &catalog,
        &given,
        span,
        release,
        flag(args, "--lc-monetary"),
    )?;
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
    // A table laid out on a key handed out as rows arrive is said to be: the DDL names no other
    // order for it.
    for word in catalog
        .tables
        .iter()
        .filter_map(|t| t.laid_out_on_arrival())
        .chain(catalog.tables.iter().flat_map(|t| t.keys_read_whole()))
    {
        println!("{word}");
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
    let others = withs(args);
    let (mut snail, held_bytes) = open(file, &others, flag(args, "--hold"))?;
    if !others.is_empty() && flag(args, "--order").is_some() {
        return Err(Error::new(
            "a round across several snails chooses its orders itself: --order is refused with --with",
        ));
    }
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
        let (answer, round) = match &mut snail {
            Opened::One(s) => s.ask_with(&sql, flag(args, "--order"), &params)?,
            Opened::Several(s) => s.ask_with(&sql, &params)?,
        };
        let elapsed_us = began.elapsed().as_micros() as u64;
        let (after, _) = rchar();
        let d = Done {
            n,
            pages_at_open: match (&snail, n) {
                (Opened::One(s), 1) => s.pages_at_open,
                (Opened::Several(s), 1) => s.pages_at_open() + round.pages_at_start,
                _ => round.pages_at_start,
            },
            counted: snail.pages_read() - read_before,
            held: snail.pages_held() - held_before,
            bytes: after - before - probe,
            elapsed_us,
            round,
        };
        match &first {
            None => {
                if args.iter().any(|a| a == "--text") {
                    save(out, &as_text(&answer)?)?;
                } else {
                    save(out, &answer)?;
                }
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
            (
                "snails",
                texts(
                    done.iter()
                        .map(|d| {
                            d.round
                                .snails
                                .iter()
                                .map(|p| {
                                    format!(
                                        "{}:{}:{}:{}",
                                        p.pages_at_start,
                                        p.column_pages_predicted,
                                        p.column_pages_held,
                                        p.column_pages_read
                                    )
                                })
                                .collect::<Vec<_>>()
                                .join(",")
                        })
                        .collect(),
                ),
            ),
        ])?;
        save(path, &batch)?;
    }
    Ok(())
}

/// The snails each `--with` names.
fn withs(args: &[String]) -> Vec<&str> {
    args.windows(2)
        .filter(|w| w[0] == "--with")
        .map(|w| w[1].as_str())
        .collect()
}

/// One snail, or several opened together.
enum Opened {
    One(Snail),
    Several(Snails),
}

impl Opened {
    fn pages_read(&self) -> u64 {
        match self {
            Opened::One(s) => s.pages_read(),
            Opened::Several(s) => s.pages_read(),
        }
    }

    fn pages_held(&self) -> u64 {
        match self {
            Opened::One(s) => s.pages_held(),
            Opened::Several(s) => s.pages_held(),
        }
    }
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

/// The snail, and the others opened with it, each holding what `--hold` gives in MiB, or
/// `HELD_BYTES`; and the bytes each holds.
fn open(file: &str, others: &[&str], hold: Option<&str>) -> Result<(Opened, u64)> {
    let paths: Vec<&Path> = std::iter::once(file)
        .chain(others.iter().copied())
        .map(Path::new)
        .collect();
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
        let opened = match others.is_empty() {
            true => Opened::One(Snail::open_holding(paths[0], bytes)?),
            false => Opened::Several(Snails::open_holding(&paths, bytes)?),
        };
        Ok((opened, bytes as u64))
    }
    #[cfg(not(feature = "standalone"))]
    {
        if hold.is_some() {
            return Err(Error::new(
                "built without `standalone`, a snail holds no pages of its own: --hold is refused",
            ));
        }
        let opened = match others.is_empty() {
            true => Opened::One(Snail::open(paths[0])?),
            false => Opened::Several(Snails::open(&paths)?),
        };
        Ok((opened, 0))
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
    for (i, p) in r.snails.iter().enumerate() {
        println!(
            "    snail {}: {} of its root as the round began, along {}; {} of columns predicted, \
             {} of them held and {} read",
            i + 1,
            n(p.pages_at_start, "page"),
            if p.order.is_empty() { "none" } else { &p.order },
            p.column_pages_predicted,
            p.column_pages_held,
            p.column_pages_read
        );
    }
}

fn cmd_catalog(args: &[String]) -> Result<()> {
    let catalog = match flag(args, "--ddl") {
        Some(path) => {
            let ddl =
                std::fs::read_to_string(path).map_err(|e| Error::new(format!("{path}: {e}")))?;
            let reading = Catalog::read(&ddl)?;
            for s in &reading.statements {
                match &s.refused {
                    None => println!("taken    {} {}", s.kind, s.object),
                    Some(why) => println!("refused  {} {}: {why}", s.kind, s.object),
                }
                for n in &s.notices {
                    println!("         {n}");
                }
            }
            if let Some(out) = flag(args, "--statements") {
                let st = &reading.statements;
                let batch = table(vec![
                    ("statement", counts((1..=st.len() as u64).collect())),
                    ("kind", texts(st.iter().map(|s| s.kind.clone()).collect())),
                    (
                        "object",
                        texts(st.iter().map(|s| s.object.clone()).collect()),
                    ),
                    (
                        "refused",
                        Arc::new(
                            st.iter()
                                .map(|s| s.refused.clone())
                                .collect::<StringArray>(),
                        ) as ArrayRef,
                    ),
                    (
                        "notices",
                        texts(st.iter().map(|s| s.notices.join("\n")).collect()),
                    ),
                    ("text", texts(st.iter().map(|s| s.text.clone()).collect())),
                ])?;
                save(out, &batch)?;
            }
            if let Some(taken) = flag(args, "--taken") {
                std::fs::write(taken, &reading.catalog.ddl)
                    .map_err(|e| Error::new(format!("{taken}: {e}")))?;
            }
            reading.catalog
        }
        None => {
            let file = args
                .first()
                .filter(|a| !a.starts_with("--"))
                .ok_or_else(|| Error::new(USAGE))?;
            let snail = Snail::open(Path::new(file))?;
            let mut catalog = snail.catalog.clone();
            // A sequence stands where the file says, which its DDL says only until it hands out a
            // value.
            for (name, last, called) in snail.sequences()? {
                if let Some(s) = catalog.sequences.iter_mut().find(|s| s.name == name) {
                    s.last_value = last;
                    s.is_called = called;
                }
            }
            catalog
        }
    };
    if let Some(out) = flag(args, "--out") {
        let f = catalog.facts();
        let batch = table(vec![
            ("kind", texts(f.iter().map(|x| x.kind.clone()).collect())),
            (
                "object",
                texts(f.iter().map(|x| x.object.clone()).collect()),
            ),
            ("fact", texts(f.iter().map(|x| x.fact.clone()).collect())),
            ("value", texts(f.iter().map(|x| x.value.clone()).collect())),
        ])?;
        save(out, &batch)?;
        println!("{out}: {}", n(f.len() as u64, "fact"));
    }
    Ok(())
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

fn cmd_functions(args: &[String]) -> Result<()> {
    use warren_snail::functions::{all, Class};
    let release = match flag(args, "--release") {
        Some(r) => r
            .parse()
            .ok()
            .filter(|r| warren_snail::regs::RELEASES.contains(r))
            .ok_or_else(|| Error::new(format!("--release {r}: the list holds 14 to 19")))?,
        None => warren_snail::regs::LATEST,
    };
    let of: Vec<_> = all().iter().filter(|f| f.in_release(release)).collect();
    let state = |f: &warren_snail::functions::Function| match (f.class, f.held()) {
        (Class::Server, _) => "refused",
        (_, true) => "held",
        _ => "not read",
    };
    for s in ["held", "not read", "refused"] {
        println!("{s:9} {}", of.iter().filter(|f| state(f) == s).count());
    }
    println!("in all    {} functions of {release}", of.len());
    if let Some(out) = flag(args, "--out") {
        let batch = table(vec![
            (
                "signature",
                texts(of.iter().map(|f| f.signature.to_string()).collect()),
            ),
            (
                "name",
                texts(of.iter().map(|f| f.name.to_string()).collect()),
            ),
            (
                "class",
                texts(
                    of.iter()
                        .map(|f| {
                            match f.class {
                                Class::Server => "server",
                                _ => "type",
                            }
                            .to_string()
                        })
                        .collect(),
                ),
            ),
            (
                "family",
                texts(of.iter().map(|f| f.family.to_string()).collect()),
            ),
            (
                "state",
                texts(of.iter().map(|f| state(f).to_string()).collect()),
            ),
        ])?;
        save(out, &batch)?;
    }
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Built with zones, SNAIL_ZONES names the directory of compiled zones read, a server's own.
    #[cfg(feature = "zones")]
    if let Some(dir) = std::env::var_os("SNAIL_ZONES") {
        warren_snail::zones::set_directory(dir.into());
    }
    let result = match args.first().map(String::as_str) {
        Some("write") => cmd_write(&args[1..]),
        Some("read") => cmd_read(&args[1..]),
        Some("exec") => cmd_exec(&args[1..]),
        Some("out") => cmd_out(&args[1..]),
        Some("hold") => cmd_hold(&args[1..]),
        Some("census") => cmd_census(&args[1..]),
        Some("recut") => cmd_recut(&args[1..]),
        Some("blocks") => cmd_blocks(&args[1..]),
        Some("catalog") => cmd_catalog(&args[1..]),
        Some("functions") => cmd_functions(&args[1..]),
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
