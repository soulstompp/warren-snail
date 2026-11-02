//! A round whose answer its host takes a batch at a time, in a room its host sizes: what it holds of
//! its answer counted in that room, the rows past it written to the host's file where the host gives
//! one, or to a file of its own, and read back exactly as they were written.

mod weather;

use arrow::array::RecordBatch;
use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use warren_snail::catalog::Catalog;
use warren_snail::read::{
    answer_together, begin_together, price_stopped, price_together, Answering, Round, Snail,
};
use warren_snail::room::{RoundFile, RoundFiles, Sizes};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use warren_snail::{Error, Result};
use weather::{fresh, tmp, write};

const A: i64 = 3_000;
const B: i64 = 16_384;

const A_DDL: &str = "CREATE TABLE s.a (id bigint NOT NULL, g bigint NOT NULL, name text NOT NULL, \
                     PRIMARY KEY (id));";
const B_DDL: &str =
    "CREATE TABLE s.b (id bigint NOT NULL, a_id bigint NOT NULL, v bigint NOT NULL, \
                     PRIMARY KEY (id)); CREATE INDEX b_a ON s.b (a_id, id) INCLUDE (v);";

fn a_rows() -> Vec<Vec<String>> {
    (0..A)
        .map(|i| vec![i.to_string(), (i % 97).to_string(), format!("name {i}")])
        .collect()
}

fn b_rows() -> Vec<Vec<String>> {
    (0..B)
        .map(|i| vec![i.to_string(), (i * 31 % A).to_string(), (B - i).to_string()])
        .collect()
}

fn rows_of(catalog: &Catalog, table: &str, rows: &[Vec<String>]) -> Rows {
    let t = catalog.table(table).unwrap();
    Rows {
        columns: t
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| {
                let mut v = Values::new(col.ty);
                for r in rows {
                    push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, &r[c]).unwrap(),
                    );
                }
                v
            })
            .collect(),
    }
}

/// Both tables as one snail.
fn one(name: &str) -> PathBuf {
    let path = tmp(&format!("handing-{name}.snail"));
    let ddl = format!("{A_DDL}\n{B_DDL}");
    let catalog = Catalog::parse(&ddl).unwrap();
    write(
        &path,
        &catalog,
        &[
            ("s.a".into(), rows_of(&catalog, "s.a", &a_rows())),
            ("s.b".into(), rows_of(&catalog, "s.b", &b_rows())),
        ],
    )
    .unwrap();
    path
}

/// Each table as a snail of its own.
fn two(name: &str) -> (PathBuf, PathBuf) {
    let a = tmp(&format!("handing-{name}-a.snail"));
    let b = tmp(&format!("handing-{name}-b.snail"));
    let catalog = Catalog::parse(A_DDL).unwrap();
    write(
        &a,
        &catalog,
        &[("s.a".into(), rows_of(&catalog, "s.a", &a_rows()))],
    )
    .unwrap();
    let catalog = Catalog::parse(B_DDL).unwrap();
    write(
        &b,
        &catalog,
        &[("s.b".into(), rows_of(&catalog, "s.b", &b_rows()))],
    )
    .unwrap();
    (a, b)
}

fn opened(paths: &[&Path]) -> Vec<Snail> {
    paths.iter().map(|p| fresh(p)).collect()
}

/// A batch's rows as text.
fn texts(b: &RecordBatch) -> Vec<String> {
    warren_snail::read::pipeline_rows(b)
        .unwrap()
        .iter()
        .map(|r| {
            r.iter()
                .map(|v| format!("{v:?}"))
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect()
}

/// Every batch a round hands out: the rows as text, sorted, how many batches, the most rows one held.
fn drained(a: &mut Answering) -> (Vec<String>, usize, usize) {
    let (mut rows, mut batches, mut most) = (Vec::new(), 0, 0);
    while let Some(b) = a.next_batch().unwrap() {
        batches += 1;
        most = most.max(b.num_rows());
        rows.extend(texts(&b));
    }
    rows.sort();
    (rows, batches, most)
}

/// The same question answered whole in the default room: its rows as text, sorted.
fn whole(snails: &[Snail], sql: &str) -> Vec<String> {
    let refs: Vec<&Snail> = snails.iter().collect();
    let (b, _) = answer_together(&refs, sql).unwrap();
    let mut rows = texts(&b);
    rows.sort();
    rows
}

/// The same value printed the same whichever path made it: an integer of a sum or a count may come
/// out as a number of scale 0.
fn alike(rows: Vec<String>) -> Vec<String> {
    let mut rows: Vec<String> = rows
        .into_iter()
        .map(|r| {
            r.replace(", 0)", ")")
                .replace("Num(", "Int(")
                .replace("Text(", "T(")
        })
        .collect();
    rows.sort();
    rows
}

const JOIN: &str = "SELECT a.id AS c1, a.name AS c2, b.id AS c3, b.v AS c4 FROM s.a a \
                    JOIN s.b b ON b.a_id = a.id";

/// Held in memory, as a host's file: its pages, how many it wrote and read, and whether it is closed.
#[derive(Default)]
struct Kept {
    pages: Vec<Vec<u8>>,
    written: u64,
    read: u64,
    made: u64,
    closed: u64,
    /// Refused from this page on, by a return or by a panic.
    refuse_at: Option<u64>,
    panics: bool,
}

struct Host(Rc<RefCell<Kept>>);

impl RoundFile for Host {
    fn write_page(&mut self, at: u64, page: &[u8]) -> Result<()> {
        let mut k = self.0.borrow_mut();
        if k.refuse_at.is_some_and(|r| at >= r) {
            let panics = k.panics;
            drop(k);
            if panics {
                std::panic::panic_any("the host's own error");
            }
            return Err(Error::new("the host refused a page"));
        }
        assert_eq!(at, k.pages.len() as u64, "each page once, in turn");
        k.pages.push(page.to_vec());
        k.written += 1;
        Ok(())
    }

    fn read_page(&mut self, at: u64, buf: &mut [u8]) -> Result<()> {
        let mut k = self.0.borrow_mut();
        buf.copy_from_slice(&k.pages[at as usize]);
        k.read += 1;
        Ok(())
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.0.borrow_mut().closed += 1;
    }
}

fn hosted(room: u64, kept: &Rc<RefCell<Kept>>) -> Sizes {
    let kept = kept.clone();
    Sizes::of(room).written_to(RoundFiles::new(move || {
        kept.borrow_mut().made += 1;
        Ok(Box::new(Host(kept.clone())) as Box<dyn RoundFile>)
    }))
}

/// A join past its room lays its answer out a batch at a time: the same rows as the whole answer in
/// the default room, what it wrote read back exactly, once.
#[test]
fn a_join_past_its_room_reads_back_exactly_what_it_wrote() {
    let path = one("join");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let want = whole(&snails, JOIN);
    assert_eq!(want.len(), B as usize);
    // The sides fit the room and the answer does not: written once, read back once.
    let mut a = begin_together(&refs, JOIN, Sizes::of(192 << 10)).unwrap();
    let (rows, batches, most) = drained(&mut a);
    assert_eq!(alike(rows), alike(want.clone()));
    let r = a.round();
    assert_eq!(r.order, "a_pkey+b_a");
    assert!(r.spill_pages_written > 0);
    assert_eq!(r.spill_pages_written, r.spill_pages_read);
    assert!(batches > 8, "{batches} batches");
    assert!(most < B as usize / 4, "{most} rows in one batch");
    // Neither side fits: met in batches, the same rows, a side's rows past the room read once for
    // the keys the other is kept to, and once to meet them.
    let mut a = begin_together(&refs, JOIN, Sizes::of(64 << 10)).unwrap();
    let (rows, _, _) = drained(&mut a);
    assert_eq!(alike(rows), alike(want));
    let r = a.round();
    assert_eq!(
        r.order, "a_pkey+a_pkey+b_a",
        "the join asked once, then taken up"
    );
    assert!(r.spill_pages_written > 0);
    assert!(r.spill_pages_read >= r.spill_pages_written);
    assert!(r.spill_pages_read <= r.spill_pages_written + 32);
    // Within the room nothing is written, its reads the whole answer's.
    let (_, whole_round): (RecordBatch, Round) = answer_together(&refs, JOIN).unwrap();
    let mut a = begin_together(&refs, JOIN, Sizes::of(128 << 20)).unwrap();
    let (rows, _, _) = drained(&mut a);
    assert_eq!(rows.len(), B as usize);
    let r = a.round();
    assert_eq!((r.spill_pages_written, r.spill_pages_read), (0, 0));
    assert_eq!(r.order, whole_round.order);
    assert_eq!(r.directory_pages, whole_round.directory_pages);
    assert_eq!(r.column_pages_predicted, whole_round.column_pages_predicted);
}

/// A join across two snails, past its room: one round, the same rows.
#[test]
fn a_join_across_two_snails_keeps_to_its_room() {
    let (a, b) = two("across");
    let snails = opened(&[&a, &b]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let want = whole(&snails, JOIN);
    assert_eq!(want.len(), B as usize);
    let mut at = begin_together(&refs, JOIN, Sizes::of(64 << 10)).unwrap();
    let (rows, _, _) = drained(&mut at);
    assert_eq!(alike(rows), alike(want));
    let r = at.round();
    assert!(r.spill_pages_written > 0);
    assert!(r.spill_pages_read >= r.spill_pages_written);
    assert_eq!(r.snails.len(), 2);
    assert!(r.bytes_held > 0);
    // Past the room by both sides, the pipeline over the snails' relations reads it.
    let mut at = begin_together(&refs, JOIN, Sizes::of(8 << 10)).unwrap();
    let (rows, _, _) = drained(&mut at);
    assert_eq!(rows.len(), B as usize);
    let r = at.round();
    assert!(r.spill_pages_read >= r.spill_pages_written);
}

/// A grouping of few groups holds them, however many rows it reads; one whose groups pass the room
/// stops there and goes on past it, its pipeline writing those past it: the same answer.
#[test]
fn a_grouping_whose_groups_pass_the_room_goes_on_past_it() {
    let path = one("groups");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let few = "SELECT g AS c1, count(*) AS c2 FROM s.a GROUP BY g";
    let want = whole(&snails, few);
    assert_eq!(want.len(), 97);
    let mut a = begin_together(&refs, few, Sizes::of(16 << 10)).unwrap();
    let (rows, batches, _) = drained(&mut a);
    assert_eq!(rows, want);
    assert_eq!(batches, 1);
    assert_eq!(a.round().spill_pages_written, 0);

    let many = "SELECT id AS c1, count(*) AS c2 FROM s.b GROUP BY id";
    let want = whole(&snails, many);
    assert_eq!(want.len(), B as usize);
    let mut a = begin_together(&refs, many, Sizes::of(32 << 10)).unwrap();
    let (rows, batches, _) = drained(&mut a);
    assert_eq!(alike(rows), alike(want));
    assert!(batches > 1, "{batches}");
    let r = a.round();
    assert!(r.spill_pages_written > 0);
    assert_eq!(r.spill_pages_written, r.spill_pages_read);
    assert!(
        r.bytes_held < B as u64 * 16,
        "{} held for {} groups",
        r.bytes_held,
        B
    );

    let joined = "SELECT b.id AS c1, count(*) AS c2 FROM s.a a JOIN s.b b ON b.a_id = a.id \
                  GROUP BY b.id";
    let want = whole(&snails, joined);
    let mut a = begin_together(&refs, joined, Sizes::of(64 << 10)).unwrap();
    let (rows, _, _) = drained(&mut a);
    assert_eq!(alike(rows), alike(want));
    let r = a.round();
    assert!(r.spill_pages_written > 0);
}

/// Given a host's file, every page past the room goes to it and comes back from it; it is made at the
/// first page, and closed as the answer is let go.
#[test]
fn a_host_s_file_takes_every_page_past_the_room() {
    let path = one("host");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let kept = Rc::new(RefCell::new(Kept::default()));
    let want = whole(&snails, JOIN);
    let mut a = begin_together(&refs, JOIN, hosted(64 << 10, &kept)).unwrap();
    assert_eq!(kept.borrow().made, 1);
    let (rows, _, _) = drained(&mut a);
    assert_eq!(alike(rows), alike(want));
    let r = a.round();
    {
        let k = kept.borrow();
        assert!(k.written > 0);
        assert_eq!(k.written, r.spill_pages_written);
        assert_eq!(k.read, r.spill_pages_read);
        assert!(k.read >= k.written);
        assert_eq!(k.closed, 0);
    }
    drop(a);
    assert_eq!(kept.borrow().closed, 1);
    // Within the room, none is made.
    let kept = Rc::new(RefCell::new(Kept::default()));
    let mut a = begin_together(&refs, JOIN, hosted(128 << 20, &kept)).unwrap();
    drained(&mut a);
    assert_eq!(kept.borrow().made, 0);
}

/// A host's file that refuses a page refuses the round by what it said, and one whose error unwinds
/// through the round leaves the snail as it was: the next round answers.
#[test]
fn a_host_s_refusal_ends_the_round_and_the_next_answers() {
    let path = one("refused");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let want = whole(&snails, JOIN);
    let kept = Rc::new(RefCell::new(Kept {
        refuse_at: Some(2),
        ..Kept::default()
    }));
    let e = match begin_together(&refs, JOIN, hosted(64 << 10, &kept)) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("the host refused a page"),
    };
    assert!(e.contains("the host refused a page"), "{e}");

    let panicking = Rc::new(RefCell::new(Kept {
        refuse_at: Some(2),
        panics: true,
        ..Kept::default()
    }));
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let done = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        begin_together(&refs, JOIN, hosted(64 << 10, &panicking)).map(|_| ())
    }));
    std::panic::set_hook(hook);
    assert!(done.is_err(), "the host's error unwinds");

    let mut a = begin_together(&refs, JOIN, Sizes::of(64 << 10)).unwrap();
    let (rows, _, _) = drained(&mut a);
    assert_eq!(alike(rows), alike(want));
}

/// A second round begun while the first's answer is handed out keeps to a room and a file of its own.
#[test]
fn a_second_round_begun_while_the_first_hands_out_takes_a_room_of_its_own() {
    let path = one("second");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let want = alike(whole(&snails, JOIN));
    let mut first = begin_together(&refs, JOIN, Sizes::of(64 << 10)).unwrap();
    let mut rows = texts(&first.next_batch().unwrap().unwrap());
    let mut second = begin_together(&refs, JOIN, Sizes::of(64 << 10)).unwrap();
    let (other, _, _) = drained(&mut second);
    assert_eq!(alike(other), want);
    while let Some(b) = first.next_batch().unwrap() {
        rows.extend(texts(&b));
    }
    assert_eq!(alike(rows), want);
    let (a, b) = (first.round(), second.round());
    assert!(a.spill_pages_written > 0);
    assert_eq!(a.spill_pages_read, b.spill_pages_read);
}

/// An answer past its room lies past it: the round holds no more than its room and a chunk or two in
/// the hands of its places, however many rows it answers.
#[test]
fn what_a_round_holds_keeps_to_its_room() {
    let path = one("most");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let room = 64 << 10;
    let mut a = begin_together(&refs, JOIN, Sizes::of(room)).unwrap();
    let (rows, _, _) = drained(&mut a);
    assert_eq!(rows.len(), B as usize);
    let r = a.round();
    assert!(
        r.bytes_held <= room + 6 * (16 << 10),
        "{} past a room of {room}",
        r.bytes_held
    );
    // Where it fits, all of it lies in the room.
    let mut a = begin_together(&refs, JOIN, Sizes::of(128 << 20)).unwrap();
    let _ = a.next_batch().unwrap();
    assert!(a.round().bytes_held > room);
}

/// Each table's bytes at most: its rows at their held width, or the bytes they take in the pages read
/// where those are more; a table read from the counts alone, none.
#[test]
fn a_table_is_priced_at_the_bytes_a_round_holds_of_it() {
    let path = one("held");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let p = price_together(&refs, "SELECT count(*) AS c1 FROM s.b").unwrap();
    assert_eq!(p.tables[0].held_bytes, 0);
    let p = price_together(
        &refs,
        "SELECT a_id AS c1, count(*) AS c2 FROM s.b GROUP BY a_id",
    )
    .unwrap();
    assert_eq!(p.tables[0].held_bytes, B as u64 * 9);
    let p = price_together(&refs, JOIN).unwrap();
    for t in &p.tables {
        // A name past its floor of 5 bytes takes what the pages give it: its bytes and their length.
        let row = t.held_bytes / t.rows_high;
        match t.table.as_str() {
            "s.a" => assert!((9 + 9..=9 + 13).contains(&row), "{row}"),
            _ => assert_eq!(row, 9 + 9 + 9),
        }
    }
}

/// With no host's file a round writes a file of its own, removed as soon as it is opened.
#[test]
fn with_no_host_its_own_file_takes_the_pages() {
    let path = one("own");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let dir = tmp("handing-own-spill");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let mut seen = 0;
    let mut a = begin_together(&refs, JOIN, Sizes::of(64 << 10).in_dir(&dir)).unwrap();
    assert_eq!(
        std::fs::read_dir(&dir).unwrap().count(),
        0,
        "removed at once"
    );
    while let Some(b) = a.next_batch().unwrap() {
        seen += b.num_rows() as u64;
    }
    assert_eq!(seen, B as u64);
    assert!(a.round().spill_pages_written > 0);
}

/// A round's Parquet, as `warren-snail read` records it: one row, a value of a count's column.
fn counted(round: &RecordBatch, column: &str) -> u64 {
    use arrow::array::{Array, UInt64Array};
    let c = round.column_by_name(column).unwrap();
    let c = c.as_any().downcast_ref::<UInt64Array>().unwrap();
    assert_eq!(c.len(), 1);
    c.value(0)
}

/// `warren-snail read` of a question: its answer's rows and its round's row, or what it was refused.
fn command_reads(
    path: &Path,
    name: &str,
    sql: &str,
    flags: &[&str],
) -> Result<(usize, RecordBatch)> {
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    let (q, answer, round) = (
        tmp(&format!("handing-{name}.sql")),
        tmp(&format!("handing-{name}-answer.parquet")),
        tmp(&format!("handing-{name}-round.parquet")),
    );
    std::fs::write(&q, sql).unwrap();
    let held: &[&str] = if cfg!(feature = "standalone") {
        &["--hold", "0"]
    } else {
        &[]
    };
    let done = std::process::Command::new(env!("CARGO_BIN_EXE_warren-snail"))
        .args([
            "read",
            path.to_str().unwrap(),
            q.to_str().unwrap(),
            "--out",
            answer.to_str().unwrap(),
            "--round",
            round.to_str().unwrap(),
        ])
        .args(held)
        .args(flags)
        .output()
        .unwrap();
    if !done.status.success() {
        return Err(Error::new(String::from_utf8_lossy(&done.stderr).trim()));
    }
    let batches = |p: &Path| {
        ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(p).unwrap())
            .unwrap()
            .build()
            .unwrap()
            .map(|b| b.unwrap())
            .collect::<Vec<_>>()
    };
    let rows = batches(&answer).iter().map(|b| b.num_rows()).sum();
    Ok((rows, batches(&round).remove(0)))
}

/// What the command recorded of a round is what the round was.
fn recorded_as(recorded: &RecordBatch, r: &Round) {
    use arrow::array::{Array, StringArray};
    let order = recorded.column_by_name("order").unwrap();
    let order = order.as_any().downcast_ref::<StringArray>().unwrap();
    assert_eq!(order.value(0), r.order);
    for (column, value) in [
        ("directory_pages", r.directory_pages),
        ("column_pages_predicted", r.column_pages_predicted),
        ("column_pages_held", r.column_pages_held),
        ("column_pages_read", r.column_pages_read),
        ("spill_pages_written", r.spill_pages_written),
        ("spill_pages_read", r.spill_pages_read),
        ("bytes_held", r.bytes_held),
    ] {
        assert_eq!(counted(recorded, column), value, "{column}");
    }
}

/// `--handed` in a room reads as a host's round in that room does, its answer whole; a grouping whose
/// groups pass the room among them, which stops where a round that does not count its answer goes on.
#[test]
fn the_command_reads_a_handed_round_as_its_host_does() {
    let path = one("command");
    let many = "SELECT id AS c1, count(*) AS c2 FROM s.b GROUP BY id";
    for (name, sql, room, rows) in [
        ("join", JOIN, 64u64 << 10, B as usize),
        ("groups", many, 32 << 10, B as usize),
    ] {
        let snail = fresh(&path);
        let mut a = begin_together(&[&snail], sql, Sizes::of(room)).unwrap();
        let (answer, batches, _) = drained(&mut a);
        assert_eq!(answer.len(), rows);
        assert!(batches > 2, "{name}: {batches}");
        let host = a.round();
        assert!(host.spill_pages_written > 0, "{name}");
        let bytes = room.to_string();
        let flags = ["--room", bytes.as_str(), "--handed"];
        let (answered, round) = command_reads(&path, name, sql, &flags).unwrap();
        assert_eq!(answered, rows, "{name}");
        recorded_as(&round, &host);
        assert_eq!(counted(&round, "pages_held"), snail.pages_held(), "{name}");
    }
    // Across two snails, each opened with `--with`.
    let (a, b) = two("command-with");
    let snails = opened(&[&a, &b]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let room = 64u64 << 10;
    let mut at = begin_together(&refs, JOIN, Sizes::of(room)).unwrap();
    assert_eq!(drained(&mut at).0.len(), B as usize);
    let host = at.round();
    assert!(host.spill_pages_written > 0);
    let bytes = room.to_string();
    let flags = [
        "--with",
        b.to_str().unwrap(),
        "--room",
        bytes.as_str(),
        "--handed",
    ];
    let (answered, round) = command_reads(&a, "with", JOIN, &flags).unwrap();
    assert_eq!(answered, B as usize);
    recorded_as(&round, &host);
    assert_eq!(
        counted(&round, "pages_held"),
        snails.iter().map(Snail::pages_held).sum::<u64>()
    );
    // Two rooms: the groups a round not counting its answer holds, where a host's stops for them.
    let at = |flags: &[&str]| command_reads(&path, "groups-at", many, flags).unwrap().1;
    let room = (32u64 << 10).to_string();
    let (plain, handed) = (at(&["--room", &room]), at(&["--room", &room, "--handed"]));
    assert_ne!(
        counted(&plain, "spill_pages_written"),
        counted(&handed, "spill_pages_written"),
    );
}

/// `--room` alone gives each round that room, as `Snail::size_rounds` does; a room is in bytes, and
/// what a handed round cannot take is refused by name.
#[test]
fn the_command_gives_a_round_the_room_it_names() {
    let path = one("command-room");
    let room = 64u64 << 10;
    let mut snail = fresh(&path);
    snail.size_rounds(Sizes::of(room));
    let (b, sized) = snail.ask(JOIN, None).unwrap();
    assert!(sized.spill_pages_written > 0);
    let (answered, round) =
        command_reads(&path, "room", JOIN, &["--room", &room.to_string()]).unwrap();
    assert_eq!(answered, b.num_rows());
    recorded_as(&round, &sized);
    // Unsized, nothing goes past it.
    let (answered, round) = command_reads(&path, "unsized", JOIN, &[]).unwrap();
    assert_eq!(answered, b.num_rows());
    assert_eq!(counted(&round, "spill_pages_written"), 0);
    for (flags, words) in [
        (&["--room", "64MB"][..], "--room takes bytes"),
        (
            &["--handed", "--order", "a_pkey"][..],
            "--order and --param are refused with --handed",
        ),
        (
            &["--handed", "--param", "1"][..],
            "--order and --param are refused with --handed",
        ),
    ] {
        let said = command_reads(&path, "refused", JOIN, flags)
            .unwrap_err()
            .to_string();
        assert!(said.contains(words), "{flags:?}: {said}");
    }
}

/// A round that stops for its room reads a side again, and its price's most counts that: at every room
/// from one that stops before its first side to one that never stops, its pages lie between the fewest
/// and the most.
#[test]
fn a_round_stopped_for_its_room_reads_no_more_than_the_most_its_price_gives() {
    let path = one("stopped");
    let snails = opened(&[&path]);
    let refs: Vec<&Snail> = snails.iter().collect();
    let by_group = "SELECT b.id AS c1, count(*) AS c2 FROM s.a a JOIN s.b b ON b.a_id = a.id \
                    GROUP BY b.id";
    let by_name = "SELECT a.g AS c1, count(*) AS c2 FROM s.a a JOIN s.b b ON b.a_id = a.id \
                   GROUP BY a.g";
    let outer = "SELECT a.id AS c1, b.v AS c2 FROM s.a a LEFT JOIN s.b b ON b.a_id = a.id";
    for (q, read_again) in [
        (JOIN, 184),
        (by_name, 133),
        (by_group, 173),
        (outer, 160),
        ("SELECT id AS c1, count(*) AS c2 FROM s.b GROUP BY id", 79),
        ("SELECT g AS c1, count(*) AS c2 FROM s.a GROUP BY g", 12),
    ] {
        let priced = price_together(&refs, q).unwrap();
        let (low, high, _) = priced.column_pages();
        let mut worst = 0;
        for room in [4u64, 8, 16, 32, 64, 128, 192, 256, 512, 1024, 128 << 10].map(|k| k << 10) {
            let most = price_stopped(&refs, q, &priced, room)
                .unwrap()
                .column_pages_most;
            assert!(most >= high, "{q}: {most} under {high}");
            let mut a = begin_together(&refs, q, Sizes::of(room)).unwrap();
            let _ = drained(&mut a);
            let r = a.round();
            assert!(
                low <= r.column_pages_predicted && r.column_pages_predicted <= most,
                "{q} in {room}: {low} to {most}, {} predicted",
                r.column_pages_predicted
            );
            assert_eq!(
                r.column_pages_read,
                r.column_pages_predicted - r.column_pages_held,
                "{q} in {room}"
            );
            worst = worst.max(r.column_pages_predicted);
        }
        // The most any of these rooms read: past `high`, where a side was read again.
        assert_eq!(worst, read_again, "{q}");
    }
    // A join met as it reads stops before its last side: the one read again is its first, exactly.
    let priced = price_together(&refs, JOIN).unwrap();
    assert_eq!(priced.column_pages().1, 162);
    assert_eq!(
        price_stopped(&refs, JOIN, &priced, 64 << 10)
            .unwrap()
            .column_pages_most,
        184
    );
    // A room that holds what the counts say it may hold is no stop.
    assert_eq!(
        price_stopped(&refs, JOIN, &priced, 128 << 20)
            .unwrap()
            .column_pages_most,
        162
    );
}
