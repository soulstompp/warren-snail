//! SERIALIZABLE as PostgreSQL has it: a transaction's reads marked on each block, the edges between
//! overlapping transactions found from both sides, and two in a row refused where PostgreSQL's
//! monitor refuses them, in its words; READ ONLY and DEFERRABLE; the records a commit leaves; the
//! older formats and a replica refused by name; and every interleaving of two transactions over two
//! blocks, within one and across two snails held to some serial order.
#![cfg(feature = "standalone")]

mod weather;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;
use warren_snail::catalog::Catalog;
use warren_snail::read::{Done, Snail, Snails};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{one, tmp, write};

const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id));";
/// The table's rows, every sixteenth id: two blocks of 1,024, so `A` lies in the first and `B` in
/// the second, and a row inserted beside either lies in its block.
const ROWS: i32 = 2048;
const A: i32 = 1600;
const B: i32 = 32000;
/// Two rows of the second block, read apart: `C` by the block's rows up to it, `D` by those after.
const C: i32 = 17600;
const D: i32 = 17616;

fn made(name: &str) -> PathBuf {
    let path = tmp(&format!("serial-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..ROWS {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &(i * 16).to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    path
}

fn open(path: &Path) -> Snail {
    Snail::open_writing(path).unwrap()
}

fn run(w: &mut Snail, sql: &str) -> Done {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn read(w: &mut Snail, id: i32) -> i64 {
    let sql = format!("SELECT sum(t.v) FROM s.t t WHERE t.id = {id}");
    one(&w.ask(&sql, None).unwrap_or_else(|e| panic!("{sql}: {e}")).0)
}

fn set(w: &mut Snail, id: i32, v: i32) -> warren_snail::Result<Done> {
    w.execute(&format!("UPDATE s.t SET v = {v} WHERE id = {id}"))
}

fn reason(e: &warren_snail::Error) -> String {
    assert_eq!(e.sqlstate(), Some("40001"), "{e}");
    let e = e.to_string();
    let mut lines = e.lines();
    assert_eq!(
        lines.next().unwrap(),
        "could not serialize access due to read/write dependencies among transactions",
        "{e}"
    );
    let detail = lines
        .next()
        .unwrap()
        .strip_prefix("DETAIL:  Reason code: ")
        .unwrap_or_else(|| panic!("{e}"))
        .to_string();
    assert_eq!(
        lines.next().unwrap(),
        "HINT:  The transaction might succeed if retried.",
        "{e}"
    );
    detail
}

#[test]
fn two_blocks_hold_the_rows_each_transaction_reads_and_writes() {
    let path = made("blocks");
    let s = Snail::open(&path).unwrap();
    let blocks: Vec<u64> = s
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.column == "id")
        .map(|r| r.rows)
        .collect();
    assert_eq!(blocks, [1024, 1024]);
}

#[test]
fn write_skew_fails_the_second_to_commit_at_its_commit() {
    let path = made("skew");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut a, A), i64::from(A));
    assert_eq!(read(&mut b, B), i64::from(B));
    set(&mut a, B, -1).unwrap();
    set(&mut b, A, -2).unwrap();
    run(&mut a, "COMMIT");
    let e = b.execute("COMMIT").unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during commit attempt."
    );
    // Only the first commit stands.
    let mut c = Snail::open(&path).unwrap();
    assert_eq!(read(&mut c, B), -1);
    assert_eq!(read(&mut c, A), i64::from(A));
}

#[test]
fn transactions_of_blocks_apart_each_commit() {
    let path = made("apart");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    read(&mut b, B);
    set(&mut a, A, -1).unwrap();
    set(&mut b, B, -2).unwrap();
    run(&mut a, "COMMIT");
    run(&mut b, "COMMIT");
    let mut c = Snail::open(&path).unwrap();
    assert_eq!((read(&mut c, A), read(&mut c, B)), (-1, -2));
}

#[test]
fn a_read_after_the_other_committed_finds_its_edge_and_fails_the_write_that_closes_it() {
    let path = made("late");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    read(&mut b, A);
    set(&mut a, B, -1).unwrap();
    run(&mut a, "COMMIT");
    // b reads what a wrote and cannot see, an edge out to a committed transaction; a read what b
    // writes next, the edge in that makes b the pivot.
    assert_eq!(read(&mut b, B), i64::from(B));
    let e = set(&mut b, A, -2).unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during write."
    );
    assert!(b
        .execute("SELECT 1")
        .unwrap_err()
        .to_string()
        .contains("current transaction is aborted"));
    run(&mut b, "ROLLBACK");
}

#[test]
fn every_serial_history_runs_to_its_end_with_no_refusal() {
    let path = made("serial");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    set(&mut a, B, -1).unwrap();
    run(&mut a, "COMMIT");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut b, B), -1);
    set(&mut b, A, -2).unwrap();
    run(&mut b, "COMMIT");
}

#[test]
fn the_read_only_anomaly_fails_the_pivot_or_the_reader() {
    let path = made("anomaly");
    let (mut pivot, mut out, mut ro) = (open(&path), open(&path), open(&path));
    run(&mut pivot, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // The pivot reads A, which out writes; it writes B, which the reader reads.
    read(&mut pivot, A);
    set(&mut pivot, B, -1).unwrap();
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    run(&mut ro, "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY");
    // The reader sees out's write, and not the pivot's: it comes after out and before the pivot,
    // which comes before out.
    assert_eq!(read(&mut ro, A), -2);
    assert_eq!(read(&mut ro, B), i64::from(B));
    run(&mut ro, "COMMIT");
    let e = pivot.execute("COMMIT").unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn read_only_takes_no_write_and_beside_no_writer_marks_nothing() {
    let path = made("read-only");
    let mut a = open(&path);
    let generation = a.generation();
    run(&mut a, "BEGIN READ ONLY");
    for (sql, what) in [
        ("UPDATE s.t SET v = 1 WHERE id = 1", "UPDATE"),
        ("DELETE FROM s.t WHERE id = 1", "DELETE"),
        ("INSERT INTO s.t (id, v) VALUES (5000, 1)", "INSERT"),
    ] {
        let e = a.execute(sql).unwrap_err().to_string();
        assert_eq!(
            e,
            format!("cannot execute {what} in a read-only transaction")
        );
        run(&mut a, "ROLLBACK");
        run(&mut a, "BEGIN READ ONLY");
    }
    let e = a
        .ask("SELECT t.id FROM s.t t WHERE t.id = 1 FOR UPDATE", None)
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "cannot execute SELECT FOR UPDATE in a read-only transaction"
    );
    run(&mut a, "ROLLBACK");
    // After a read, READ WRITE and a level are refused; READ ONLY again is taken.
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY");
    read(&mut a, A);
    run(&mut a, "SET TRANSACTION READ ONLY");
    let e = a.execute("SET TRANSACTION READ WRITE").unwrap_err();
    assert_eq!(
        e.to_string(),
        "transaction read-write mode must be set before any query"
    );
    run(&mut a, "ROLLBACK");
    // A read-only transaction beside no writer writes nothing at its commit.
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE, READ ONLY");
    read(&mut a, A);
    read(&mut a, B);
    assert_eq!(run(&mut a, "COMMIT").generation, generation);
    // Nor does a transaction that only read, beside no serializable writer.
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    assert_eq!(run(&mut a, "COMMIT").generation, generation);
    // Beside one, it writes them.
    let mut b = open(&path);
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut b, B);
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    assert_eq!(run(&mut a, "COMMIT").generation, generation + 1);
    run(&mut b, "COMMIT");
    // A session's modes: each statement alone is a transaction of its own.
    run(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
    );
    let e = set(&mut a, 1, 1).unwrap_err();
    assert_eq!(
        e.to_string(),
        "cannot execute UPDATE in a read-only transaction"
    );
    assert_eq!(e.sqlstate(), Some("25006"));
    run(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
    );
    set(&mut a, 1, 1).unwrap();
}

#[test]
fn deferrable_waits_for_a_safe_snapshot_and_then_marks_nothing() {
    let path = made("deferrable");
    let mut w = open(&path);
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, A);
    let p = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut d = open(&p);
        d.execute("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE")
            .unwrap();
        tx.send(read(&mut d, B)).unwrap();
        d.execute("COMMIT").unwrap();
    });
    // The first read waits while the writer is open, on the snapshot it took first: safe, it is kept.
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    set(&mut w, B, -7).unwrap();
    run(&mut w, "COMMIT");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10)).unwrap(),
        i64::from(B)
    );
    t.join().unwrap();
    // DEFERRABLE is set before any query, and a transaction not READ ONLY takes it and waits for
    // nothing.
    run(&mut w, "BEGIN");
    read(&mut w, A);
    assert_eq!(
        w.execute("SET TRANSACTION DEFERRABLE")
            .unwrap_err()
            .to_string(),
        "SET TRANSACTION [NOT] DEFERRABLE must be called before any query"
    );
    run(&mut w, "ROLLBACK");
    let mut other = open(&path);
    run(&mut other, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut other, A);
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE DEFERRABLE");
    read(&mut w, A);
    run(&mut w, "COMMIT");
    run(&mut other, "COMMIT");
}

#[test]
fn a_replica_refuses_the_level_as_a_hot_standby_does() {
    let path = made("replica");
    let mut a = open(&path);
    a.mark_replica(Some("primary")).unwrap();
    let e = a
        .execute("BEGIN ISOLATION LEVEL SERIALIZABLE")
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "cannot use serializable mode in a hot standby\nHINT:  You can use REPEATABLE READ instead."
    );
    assert_eq!(warren_snail::Error::new(e).sqlstate(), Some("0A000"));
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    let e = a
        .execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap_err()
        .to_string();
    assert!(
        e.starts_with("cannot use serializable mode in a hot standby"),
        "{e}"
    );
    run(&mut a, "ROLLBACK");
    run(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    run(&mut a, "BEGIN");
    let sql = "SELECT count(*) FROM s.t t";
    let e = a.ask(sql, None).unwrap_err().to_string();
    assert_eq!(
        e,
        "cannot use serializable mode in a hot standby\nDETAIL:  \
         \"default_transaction_isolation\" is set to \"serializable\".\nHINT:  You can use \
         \"SET default_transaction_isolation = 'repeatable read'\" to change the default."
    );
}

/// One of a transaction's statements in the walk: a read of a block, its rows' sum and count; a write
/// of one of its rows; a row inserted into it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Step {
    Read(i32),
    Write(i32),
    Insert(i32),
}

impl Step {
    fn swapped(self) -> Step {
        let other = |x: i32| match x {
            A => B,
            B => A,
            C => D,
            _ => C,
        };
        match self {
            Step::Read(x) => Step::Read(other(x)),
            Step::Write(x) => Step::Write(other(x)),
            Step::Insert(x) => Step::Insert(other(x)),
        }
    }
}

/// The value transaction `t`'s statement `k` writes, and the row it inserts into block `x`.
fn value_of(t: usize, k: usize) -> i32 {
    -1000 * (t as i32 + 1) - k as i32
}

fn inserted_at(x: i32, t: usize, k: usize) -> i32 {
    x + 1 + 3 * t as i32 + k as i32
}

fn range_of(x: i32) -> (i32, i32) {
    match x {
        A => (0, 16_383),
        B => (16_384, 16 * ROWS),
        C => (16_384, C + 8),
        _ => (C + 9, 16 * ROWS),
    }
}

/// The table a walk's row `x` lies in: on several snails, `B`'s block lies in a snail of its own.
fn table_of(x: i32, several: bool) -> &'static str {
    match several && x == B {
        true => "o.t",
        false => "s.t",
    }
}

fn statement(step: Step, t: usize, k: usize, several: bool) -> String {
    match step {
        Step::Read(x) => {
            let (a, z) = range_of(x);
            let table = table_of(x, several);
            format!("SELECT sum(t.v), count(*) FROM {table} t WHERE t.id >= {a} AND t.id <= {z}")
        }
        Step::Write(x) => format!(
            "UPDATE {} SET v = {} WHERE id = {x}",
            table_of(x, several),
            value_of(t, k)
        ),
        Step::Insert(x) => format!(
            "INSERT INTO {} (id, v) VALUES ({}, {})",
            table_of(x, several),
            inserted_at(x, t, k),
            value_of(t, k)
        ),
    }
}

/// The rows a history touched, over the table as it was written.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Rows2 {
    touched: BTreeMap<i32, i32>,
}

impl Rows2 {
    fn apply(&mut self, step: Step, t: usize, k: usize) {
        match step {
            Step::Read(_) => {}
            Step::Write(x) => {
                self.touched.insert(x, value_of(t, k));
            }
            Step::Insert(x) => {
                self.touched.insert(inserted_at(x, t, k), value_of(t, k));
            }
        }
    }

    /// What a read of block `x` answers.
    fn read(&self, x: i32) -> (i64, i64) {
        let (a, z) = range_of(x);
        let written = (0..ROWS).map(|i| i * 16).filter(|i| (a..=z).contains(i));
        let mut sum: i64 = written.clone().map(i64::from).sum();
        let mut count = written.count() as i64;
        for (&id, &v) in &self.touched {
            if !(a..=z).contains(&id) {
                continue;
            }
            if id % 16 == 0 {
                sum += i64::from(v) - i64::from(id);
            } else {
                sum += i64::from(v);
                count += 1;
            }
        }
        (sum, count)
    }
}

/// What a read answered: a block's sum and count; none for a statement that reads nothing of its own.
type Answered = Option<(i64, i64)>;

/// Whether a transaction committed, and what each of its statements answered.
type Ran = (bool, Vec<Answered>);

/// What one step of a transaction came to: its read's answer, or none, or the words of its failure.
type Came = Result<Answered, String>;

/// A writer of one snail, or of several opened together.
enum Session {
    One(Snail),
    Several(Snails),
}

impl Session {
    fn open(paths: &[PathBuf]) -> Session {
        match paths {
            [one] => Session::One(Snail::open_writing(one).unwrap()),
            _ => {
                let refs: Vec<&Path> = paths.iter().map(PathBuf::as_path).collect();
                Session::Several(Snails::open_writing(&refs).unwrap())
            }
        }
    }

    fn ask(
        &mut self,
        sql: &str,
    ) -> warren_snail::Result<(arrow::array::RecordBatch, warren_snail::read::Round)> {
        match self {
            Session::One(s) => s.ask(sql, None),
            Session::Several(s) => s.ask(sql),
        }
    }

    fn execute(&mut self, sql: &str) -> warren_snail::Result<Done> {
        match self {
            Session::One(s) => s.execute(sql),
            Session::Several(s) => s.execute(sql),
        }
    }
}

/// A transaction on a thread of its own: statements in, outcomes out.
fn transaction(paths: Vec<PathBuf>) -> (Sender<String>, Receiver<Came>) {
    let (to, rx) = channel::<String>();
    let (tx, from) = channel();
    std::thread::spawn(move || {
        let mut s = Session::open(&paths);
        for st in rx {
            let came = if st.starts_with("SELECT") {
                s.ask(&st)
                    .map(|(b, _)| {
                        let column = |c: usize| {
                            let a = b.column(c);
                            let a = a
                                .as_any()
                                .downcast_ref::<arrow::array::Int64Array>()
                                .expect("a sum or a count");
                            match arrow::array::Array::is_valid(a, 0) {
                                true => a.value(0),
                                false => 0,
                            }
                        };
                        Some((column(0), column(1)))
                    })
                    .map_err(|e| e.to_string())
            } else {
                s.execute(&st)
                    .map(|d| {
                        // A COMMIT of a transaction that failed rolls it back.
                        if st == "COMMIT" && d.statement == "ROLLBACK" {
                            Some((-1, -1))
                        } else {
                            None
                        }
                    })
                    .map_err(|e| e.to_string())
            };
            if tx.send(came).is_err() {
                break;
            }
        }
    });
    (to, from)
}

/// Runs one interleaving on a copy of `base`. `order` names the transaction of each step, a
/// transaction's statements then its COMMIT. Returns, for each transaction, whether it committed and
/// what each of its reads answered, and the rows after; panics on a failure no serializable
/// transaction meets.
fn run_interleaving(
    base: &[PathBuf],
    at: &[PathBuf],
    transactions: &[Vec<Step>],
    order: &[u8],
    level: &str,
) -> (Vec<Ran>, [(i64, i64); 2]) {
    for (b, a) in base.iter().zip(at) {
        std::fs::copy(b, a).unwrap();
    }
    let several = at.len() > 1;
    let seen: Vec<std::fs::File> = at
        .iter()
        .map(|a| std::fs::File::open(a).unwrap())
        .collect();
    let n = transactions.len();
    let sessions: Vec<_> = (0..n).map(|_| transaction(at.to_vec())).collect();
    let mut answers: Vec<Vec<Answered>> = vec![Vec::new(); n];
    let mut failed = vec![false; n];
    let mut committed = vec![false; n];
    let mut next = vec![0; n];
    let mut pending: Vec<Vec<usize>> = vec![Vec::new(); n];
    for s in &sessions {
        s.0.send(format!("BEGIN ISOLATION LEVEL {level}")).unwrap();
        assert!(s.1.recv().unwrap().is_ok());
    }
    let settle = |pending: &mut Vec<Vec<usize>>,
                  answers: &mut Vec<Vec<Answered>>,
                  failed: &mut Vec<bool>,
                  committed: &mut Vec<bool>,
                  to_the_end: bool| {
        let started = std::time::Instant::now();
        loop {
            for (t, s) in sessions.iter().enumerate() {
                while let Ok(came) = s.1.try_recv() {
                    let k = pending[t].remove(0);
                    let is_commit = k == transactions[t].len();
                    match came {
                        Ok(Some((-1, -1))) if is_commit => failed[t] = true,
                        Ok(answer) if is_commit => {
                            let _ = answer;
                            committed[t] = true;
                        }
                        Ok(answer) => answers[t].push(answer),
                        Err(e) => {
                            assert!(
                                e.starts_with("could not serialize access")
                                    || e.starts_with("deadlock detected")
                                    || e.contains("current transaction is aborted"),
                                "{e}"
                            );
                            failed[t] = true;
                        }
                    }
                }
            }
            let running = pending.iter().filter(|p| !p.is_empty()).count() as u64;
            let waiting: u64 = seen
                .iter()
                .map(|f| warren_snail::lock::waiting(f).unwrap())
                .sum();
            if running == 0 || (!to_the_end && running == waiting) {
                return;
            }
            assert!(
                started.elapsed() < Duration::from_secs(60),
                "a step stands past a minute"
            );
            std::thread::yield_now();
        }
    };
    for &t in order {
        let t = usize::from(t);
        let k = next[t];
        next[t] += 1;
        let st = match transactions[t].get(k) {
            Some(step) => statement(*step, t, k, several),
            None => "COMMIT".to_string(),
        };
        pending[t].push(k);
        sessions[t].0.send(st).unwrap();
        settle(
            &mut pending,
            &mut answers,
            &mut failed,
            &mut committed,
            false,
        );
    }
    settle(
        &mut pending,
        &mut answers,
        &mut failed,
        &mut committed,
        true,
    );
    drop(sessions);
    let refs: Vec<&Path> = at.iter().map(PathBuf::as_path).collect();
    let mut after = Snails::open(&refs).unwrap();
    let mut block = |x: i32| {
        let (a, z) = range_of(x);
        let table = table_of(x, several);
        let (b, _) = after
            .ask(&format!(
                "SELECT sum(t.v), count(*) FROM {table} t WHERE t.id >= {a} AND t.id <= {z}"
            ))
            .unwrap();
        let v = |c: usize| {
            b.column(c)
                .as_any()
                .downcast_ref::<arrow::array::Int64Array>()
                .unwrap()
                .value(0)
        };
        (v(0), v(1))
    };
    let rows = [block(A), block(B)];
    (committed.into_iter().zip(answers).collect(), rows)
}

/// Whether the committed transactions, in some serial order, answer as they answered and leave the
/// rows as they stand.
fn some_serial_order(transactions: &[Vec<Step>], ran: &[Ran], rows: [(i64, i64); 2]) -> bool {
    fn orders(items: &mut Vec<usize>, k: usize, out: &mut Vec<Vec<usize>>) {
        if k == items.len() {
            out.push(items.clone());
            return;
        }
        for i in k..items.len() {
            items.swap(k, i);
            orders(items, k + 1, out);
            items.swap(k, i);
        }
    }
    let mut committed: Vec<usize> = (0..ran.len()).filter(|&t| ran[t].0).collect();
    let mut all = Vec::new();
    orders(&mut committed, 0, &mut all);
    all.iter().any(|order| {
        let mut state = Rows2::default();
        for &t in order {
            let mut answer = ran[t].1.iter();
            for (k, step) in transactions[t].iter().enumerate() {
                if let Step::Read(x) = step {
                    match answer.next() {
                        Some(Some(a)) if *a == state.read(*x) => {}
                        _ => return false,
                    }
                } else {
                    answer.next();
                }
                state.apply(*step, t, k);
            }
        }
        [state.read(A), state.read(B)] == rows
    })
}

/// Every order of a transaction's steps among the others', each transaction's own in place.
fn interleavings(steps: &[usize]) -> Vec<Vec<u8>> {
    fn go(left: &mut Vec<usize>, now: &mut Vec<u8>, out: &mut Vec<Vec<u8>>) {
        if left.iter().all(|&n| n == 0) {
            out.push(now.clone());
            return;
        }
        for t in 0..left.len() {
            if left[t] > 0 {
                left[t] -= 1;
                now.push(t as u8);
                go(left, now, out);
                now.pop();
                left[t] += 1;
            }
        }
    }
    let mut out = Vec::new();
    go(&mut steps.to_vec(), &mut Vec::new(), &mut out);
    out
}

/// Every transaction of one statement of `statements`, or two different ones.
fn shapes(statements: &[Step]) -> Vec<Vec<Step>> {
    let mut out: Vec<Vec<Step>> = statements.iter().map(|s| vec![*s]).collect();
    for a in statements {
        for b in statements {
            if a != b {
                out.push(vec![*a, *b]);
            }
        }
    }
    out
}

/// Every set of `n` transactions of those shapes, a set and its image with the blocks swapped once.
fn sets(all: &[Vec<Step>], n: usize) -> Vec<Vec<Vec<Step>>> {
    fn go(
        all: &[Vec<Step>],
        from: usize,
        n: usize,
        now: &mut Vec<usize>,
        out: &mut Vec<Vec<usize>>,
    ) {
        if now.len() == n {
            out.push(now.clone());
            return;
        }
        for i in from..all.len() {
            now.push(i);
            go(all, i, n, now, out);
            now.pop();
        }
    }
    let mut chosen = Vec::new();
    go(all, 0, n, &mut Vec::new(), &mut chosen);
    let swap: Vec<usize> = all
        .iter()
        .map(|s| {
            let t: Vec<Step> = s.iter().map(|x| x.swapped()).collect();
            all.iter().position(|o| *o == t).unwrap()
        })
        .collect();
    chosen
        .into_iter()
        .filter(|set| {
            let mut image: Vec<usize> = set.iter().map(|&i| swap[i]).collect();
            image.sort_unstable();
            *set <= image
        })
        .map(|set| set.into_iter().map(|i| all[i].clone()).collect())
        .collect()
}

/// What a walk found.
#[derive(Debug, Default)]
struct Walked {
    schedules: u64,
    committed: u64,
    refused: u64,
    /// A history no serial order gives, the first few.
    broken: Vec<String>,
    broken_count: u64,
}

/// Walks every interleaving of every set of `n` transactions of `statements`, at `level`, those of
/// `shard` among `of`.
fn walk(
    n: usize,
    statements: &[Step],
    level: &str,
    shard_of: (usize, usize),
    jobs: usize,
) -> Walked {
    let tag = format!(
        "{n}-{}-{}",
        level.replace(' ', "-"),
        i32::from(statements.contains(&Step::Read(C)))
    );
    walk_on(vec![made(&format!("walk-base-{tag}"))], n, statements, level, shard_of, jobs)
}

/// The same on the snails `base`, each of its own.
fn walk_on(
    base: Vec<PathBuf>,
    n: usize,
    statements: &[Step],
    level: &str,
    (shard, of): (usize, usize),
    jobs: usize,
) -> Walked {
    let tag = format!(
        "{n}-{}-{}-{}",
        level.replace(' ', "-"),
        i32::from(statements.contains(&Step::Read(C))),
        base.len()
    );
    let mut work: Vec<(Vec<Vec<Step>>, Vec<u8>)> = Vec::new();
    let mut count = 0;
    for set in sets(&shapes(statements), n) {
        let steps: Vec<usize> = set.iter().map(|t| t.len() + 1).collect();
        for order in interleavings(&steps) {
            if count % of == shard {
                work.push((set.clone(), order));
            }
            count += 1;
        }
    }
    let at = AtomicUsize::new(0);
    let found = Mutex::new(Walked::default());
    let dir = if Path::new("/dev/shm").is_dir() {
        PathBuf::from("/dev/shm")
    } else {
        PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
    };
    std::thread::scope(|scope| {
        for job in 0..jobs {
            let (work, at, found, base, dir, tag) = (&work, &at, &found, &base, &dir, &tag);
            scope.spawn(move || {
                let files: Vec<PathBuf> = (0..base.len())
                    .map(|b| {
                        dir.join(format!(
                            "warren-snail-serial-{}-{job}-{tag}-{b}.snail",
                            std::process::id()
                        ))
                    })
                    .collect();
                loop {
                    let i = at.fetch_add(1, Ordering::Relaxed);
                    let Some((set, order)) = work.get(i) else {
                        break;
                    };
                    let (ran, rows) = run_interleaving(base, &files, set, order, level);
                    let serial = some_serial_order(set, &ran, rows);
                    let mut f = found.lock().unwrap();
                    f.schedules += 1;
                    f.committed += ran.iter().filter(|r| r.0).count() as u64;
                    f.refused += ran.iter().filter(|r| !r.0).count() as u64;
                    if !serial {
                        f.broken_count += 1;
                        if f.broken.len() < 5 {
                            f.broken.push(format!("{set:?} {order:?} {ran:?} {rows:?}"));
                        }
                    }
                }
                for file in &files {
                    let _ = std::fs::remove_file(file);
                }
            });
        }
    });
    found.into_inner().unwrap()
}

const READS_AND_WRITES: [Step; 4] = [Step::Read(A), Step::Read(B), Step::Write(A), Step::Write(B)];
const AND_INSERTS: [Step; 6] = [
    Step::Read(A),
    Step::Read(B),
    Step::Write(A),
    Step::Write(B),
    Step::Insert(A),
    Step::Insert(B),
];

/// The same over one block: each transaction's rows lie in the second block, read apart.
const IN_ONE_BLOCK: [Step; 4] = [Step::Read(C), Step::Read(D), Step::Write(C), Step::Write(D)];
const IN_ONE_BLOCK_AND_INSERTS: [Step; 6] = [
    Step::Read(C),
    Step::Read(D),
    Step::Write(C),
    Step::Write(D),
    Step::Insert(C),
    Step::Insert(D),
];

/// How much of a walk a run takes: `SNAIL_SERIAL_SHARD` as `k/n`, one in `n` of its schedules.
fn shard() -> (usize, usize) {
    std::env::var("SNAIL_SERIAL_SHARD")
        .ok()
        .and_then(|s| {
            let (k, n) = s.split_once('/')?;
            Some((k.parse().ok()?, n.parse().ok()?))
        })
        .unwrap_or((0, 1))
}

fn jobs() -> usize {
    std::env::var("SNAIL_SERIAL_JOBS")
        .ok()
        .and_then(|j| j.parse().ok())
        .unwrap_or(4)
}

#[test]
fn every_interleaving_of_two_transactions_over_two_blocks_is_some_serial_order() {
    let w = walk(2, &AND_INSERTS, "SERIALIZABLE", (0, 1), jobs());
    println!("{w:?}");
    assert_eq!(w.broken_count, 0, "{:#?}", w.broken);
    assert_eq!(w.schedules, 5772);
    assert!(w.committed > 0 && w.refused > 0, "{w:?}");
}

#[test]
fn at_repeatable_read_the_same_walk_finds_histories_no_serial_order_gives() {
    let w = walk(2, &READS_AND_WRITES, "REPEATABLE READ", (0, 1), jobs());
    println!("{w:?}");
    assert!(w.broken_count > 0, "{w:?}");
}

#[test]
fn every_interleaving_of_two_transactions_in_one_block_is_some_serial_order() {
    let w = walk(2, &IN_ONE_BLOCK_AND_INSERTS, "SERIALIZABLE", (0, 1), jobs());
    println!("{w:?}");
    assert_eq!(w.broken_count, 0, "{:#?}", w.broken);
    assert_eq!(w.schedules, 5772);
    assert!(w.committed > 0 && w.refused > 0, "{w:?}");
}

#[test]
fn at_repeatable_read_the_walk_in_one_block_finds_histories_no_serial_order_gives() {
    let w = walk(2, &IN_ONE_BLOCK, "REPEATABLE READ", (0, 1), jobs());
    println!("{w:?}");
    assert!(w.broken_count > 0, "{w:?}");
}

/// Every order of two transactions writing rows of their own in one block, each of one or two
/// statements: every one commits, at `level`, and the rows after are every row's.
fn own_rows_all_commit(firsts: &[Vec<Step>], seconds: &[Vec<Step>], level: &str) {
    let base = [made(&format!("own-rows-{}", level.replace(' ', "-")))];
    let at = [tmp(&format!(
        "serial-own-rows-{}-run.snail",
        level.replace(' ', "-")
    ))];
    for a in firsts {
        for b in seconds {
            let set = vec![a.clone(), b.clone()];
            for order in interleavings(&[a.len() + 1, b.len() + 1]) {
                let (ran, rows) = run_interleaving(&base, &at, &set, &order, level);
                assert!(
                    ran.iter().all(|r| r.0),
                    "{level}: {set:?} {order:?} {ran:?}"
                );
                assert!(
                    some_serial_order(&set, &ran, rows),
                    "{level}: {set:?} {order:?} {ran:?} {rows:?}"
                );
            }
        }
    }
}

#[test]
fn writers_of_rows_of_their_own_in_one_block_all_commit_in_every_order() {
    own_rows_all_commit(
        &[
            vec![Step::Write(C)],
            vec![Step::Insert(C)],
            vec![Step::Write(C), Step::Insert(C)],
            vec![Step::Read(C), Step::Write(C)],
        ],
        &[
            vec![Step::Write(D)],
            vec![Step::Insert(D)],
            vec![Step::Insert(D), Step::Write(D)],
            vec![Step::Read(D), Step::Write(D)],
        ],
        "REPEATABLE READ",
    );
    // INSERTs read nothing, so at SERIALIZABLE the block's marks meet nothing.
    own_rows_all_commit(
        &[vec![Step::Insert(C)]],
        &[vec![Step::Insert(D)]],
        "SERIALIZABLE",
    );
}

/// The certificate's own walk, `serial.nu`: three transactions, at SERIALIZABLE.
#[test]
#[ignore]
fn every_interleaving_of_three_transactions_over_two_blocks_is_some_serial_order() {
    let w = walk(3, &READS_AND_WRITES, "SERIALIZABLE", shard(), jobs());
    println!("{w:?}");
    assert_eq!(w.broken_count, 0, "{:#?}", w.broken);
    assert!(w.schedules > 0 && w.committed > 0, "{w:?}");
}

/// The same, every transaction's rows in one block.
#[test]
#[ignore]
fn every_interleaving_of_three_transactions_in_one_block_is_some_serial_order() {
    let w = walk(3, &IN_ONE_BLOCK, "SERIALIZABLE", shard(), jobs());
    println!("{w:?}");
    assert_eq!(w.broken_count, 0, "{:#?}", w.broken);
    assert!(w.schedules > 0 && w.committed > 0, "{w:?}");
}

/// Its seeded pass: at REPEATABLE READ three transactions break.
#[test]
#[ignore]
fn at_repeatable_read_three_transactions_break() {
    let w = walk(3, &READS_AND_WRITES, "REPEATABLE READ", shard(), jobs());
    println!("{w:?}");
    assert!(w.broken_count > 0, "{w:?}");
}

/// The same, every transaction's rows in one block.
#[test]
#[ignore]
fn at_repeatable_read_three_transactions_in_one_block_break() {
    let w = walk(3, &IN_ONE_BLOCK, "REPEATABLE READ", shard(), jobs());
    println!("{w:?}");
    assert!(w.broken_count > 0, "{w:?}");
}

#[test]
fn the_older_formats_refuse_the_level_by_name_with_the_way_across() {
    let catalog = Catalog::parse(DDL).unwrap();
    let rows = || {
        let table = catalog.table("s.t").unwrap();
        Rows {
            columns: table
                .columns
                .iter()
                .map(|c| {
                    let mut v = Values::new(c.ty);
                    push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(c.ty, "1").unwrap(),
                    );
                    v
                })
                .collect(),
        }
    };
    for (format, path) in [
        (8, tmp("serial-format-eight.snail")),
        (7, tmp("serial-format-seven.snail")),
    ] {
        let tables = [("s.t".to_string(), rows())];
        match format {
            8 => warren_snail::write::write_format_eight(&path, &catalog, &tables, weather::span()),
            _ => warren_snail::write::write_format_seven(&path, &catalog, &tables, weather::span()),
        }
        .unwrap();
        let mut w = open(&path);
        let said = format!(
            "a snail of format {format} runs no SERIALIZABLE transaction, its root holding no \
             records of one: put the snail `out` and `write` it afresh, in format 9"
        );
        for sql in [
            "BEGIN ISOLATION LEVEL SERIALIZABLE",
            "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
        ] {
            assert_eq!(w.execute(sql).unwrap_err().to_string(), said, "{sql}");
        }
        run(&mut w, "BEGIN");
        assert_eq!(
            w.execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
                .unwrap_err()
                .to_string(),
            said
        );
        run(&mut w, "ROLLBACK");
        // The levels it holds no records for are taken, READ ONLY and DEFERRABLE among them.
        run(
            &mut w,
            "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY DEFERRABLE",
        );
        assert_eq!(read(&mut w, 1), 1);
        run(&mut w, "COMMIT");
    }
}

/// Two snails, `s.t` as `made` holds it and `o.t` the same rows.
fn two_snails(name: &str) -> (PathBuf, PathBuf) {
    let one = made(&format!("{name}-one"));
    let path = tmp(&format!("serial-{name}-two.snail"));
    let catalog = Catalog::parse(
        "CREATE TABLE o.t (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id));",
    )
    .unwrap();
    let table = catalog.table("o.t").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..ROWS {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &(i * 16).to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("o.t".into(), Rows { columns })]).unwrap();
    (one, path)
}

fn both(one: &Path, two: &Path) -> Snails {
    Snails::open_writing(&[one, two]).unwrap()
}

fn run_across(w: &mut Snails, sql: &str) -> Done {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
}

fn read_across(w: &mut Snails, table: &str, id: i32) -> i64 {
    let sql = format!("SELECT sum(t.v) FROM {table} t WHERE t.id = {id}");
    one(&w.ask(&sql).unwrap_or_else(|e| panic!("{sql}: {e}")).0)
}

#[test]
fn a_cycle_through_two_snails_is_refused_in_postgresql_s_words() {
    let (p, q) = two_snails("cycle");
    let (mut a, mut b) = (both(&p, &q), both(&p, &q));
    run_across(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run_across(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read_across(&mut a, "s.t", A), i64::from(A));
    assert_eq!(read_across(&mut b, "o.t", B), i64::from(B));
    run_across(&mut a, &format!("UPDATE o.t SET v = -1 WHERE id = {B}"));
    run_across(&mut b, &format!("UPDATE s.t SET v = -2 WHERE id = {A}"));
    run_across(&mut a, "COMMIT");
    assert_eq!(
        reason(&b.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
    let mut c = both(&p, &q);
    assert_eq!(read_across(&mut c, "o.t", B), -1);
    assert_eq!(read_across(&mut c, "s.t", A), i64::from(A));
}

#[test]
fn transactions_of_blocks_apart_on_two_snails_each_commit_and_read_one_moment() {
    let (p, q) = two_snails("apart-across");
    let (mut a, mut b, mut x) = (both(&p, &q), both(&p, &q), both(&p, &q));
    for level in ["REPEATABLE READ", "SERIALIZABLE"] {
        run_across(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
        run_across(&mut b, &format!("BEGIN ISOLATION LEVEL {level}"));
        let (s, o) = (read_across(&mut a, "s.t", A), read_across(&mut a, "o.t", A));
        read_across(&mut b, "s.t", B);
        // A commit outside them lands on both; neither transaction's later reads see it.
        run_across(&mut x, &format!("UPDATE s.t SET v = v - 1 WHERE id = {A}"));
        run_across(&mut x, &format!("UPDATE o.t SET v = v - 1 WHERE id = {A}"));
        assert_eq!((read_across(&mut a, "s.t", A), read_across(&mut a, "o.t", A)), (s, o));
        run_across(&mut a, &format!("UPDATE o.t SET v = -3 WHERE id = {B}"));
        run_across(&mut b, &format!("UPDATE s.t SET v = -4 WHERE id = {B}"));
        assert_eq!(read_across(&mut a, "o.t", B), -3);
        assert_eq!(read_across(&mut a, "s.t", A), s);
        run_across(&mut a, "COMMIT");
        run_across(&mut b, "COMMIT");
        let mut c = both(&p, &q);
        assert_eq!(read_across(&mut c, "o.t", B), -3);
        assert_eq!(read_across(&mut c, "s.t", B), -4);
    }
}

#[test]
fn a_transaction_across_two_snails_commits_one_root_deciding_its_records_in_each() {
    let (p, q) = two_snails("deciding");
    let (mut a, mut other) = (both(&p, &q), both(&p, &q));
    run_across(&mut other, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read_across(&mut other, "s.t", 3200);
    run_across(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run_across(&mut a, &format!("UPDATE s.t SET v = -1 WHERE id = {A}"));
    run_across(&mut a, &format!("UPDATE o.t SET v = -1 WHERE id = {B}"));
    let (g, h) = (Snail::open(&p).unwrap().generation(), Snail::open(&q).unwrap().generation());
    run_across(&mut a, "COMMIT");
    assert_eq!(
        (Snail::open(&p).unwrap().generation(), Snail::open(&q).unwrap().generation()),
        (g + 1, h + 1)
    );
    // The deciding root names the other's; every snail's root holds its record.
    assert_eq!(Snail::open(&p).unwrap().serial_records_kept().unwrap(), 1);
    assert_eq!(Snail::open(&q).unwrap().serial_records_kept().unwrap(), 1);
    run_across(&mut other, "ROLLBACK");
}

#[test]
fn a_snail_a_transaction_only_read_keeps_its_record_for_a_later_writer_there() {
    let (p, q) = two_snails("read-there");
    let (mut a, mut b) = (both(&p, &q), both(&p, &q));
    run_across(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run_across(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // a reads s.t and writes o.t; b reads o.t, then, a committed, writes what a read.
    read_across(&mut a, "s.t", A);
    read_across(&mut b, "o.t", B);
    run_across(&mut a, &format!("UPDATE o.t SET v = -1 WHERE id = {B}"));
    run_across(&mut a, "COMMIT");
    let e = b
        .execute(&format!("UPDATE s.t SET v = -2 WHERE id = {A}"))
        .unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during write."
    );
    run_across(&mut b, "ROLLBACK");
}

#[test]
fn writers_of_one_snail_beside_a_writer_of_two_meet_through_the_files_they_share() {
    let (p, q) = two_snails("beside");
    // The pivot runs on both; a reader of what it writes lies in one, the writer of what it reads in
    // the other.
    let (mut pivot, mut reader, mut out) = (both(&p, &q), open(&p), open(&q));
    run_across(&mut pivot, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut reader, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read_across(&mut pivot, "o.t", B);
    run_across(&mut pivot, &format!("UPDATE s.t SET v = -1 WHERE id = {A}"));
    assert_eq!(read(&mut reader, A), i64::from(A));
    let o = "UPDATE o.t SET v = -2 WHERE id = 32000";
    run(&mut out, o);
    run(&mut out, "COMMIT");
    // The reader, on a file the edge out never lay in, reached it through the pivot; its commit dooms
    // nothing, and the pivot fails at its commit, as a pivot whose edge out committed first.
    run(&mut reader, "COMMIT");
    assert_eq!(
        reason(&pivot.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_reader_of_one_snail_finds_an_edge_lying_in_a_file_it_never_opened() {
    let (p, q) = two_snails("reached");
    let (mut pivot, mut reader, mut out) = (both(&p, &q), open(&p), open(&q));
    run_across(&mut pivot, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut reader, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // The pivot writes what the reader will read, and has read what `out` writes: the edge out is
    // `out`'s to find, in its own file, which the reader never opened.
    run_across(&mut pivot, &format!("UPDATE s.t SET v = -1 WHERE id = {A}"));
    read_across(&mut pivot, "o.t", B);
    run(&mut out, "UPDATE o.t SET v = -2 WHERE id = 32000");
    run(&mut out, "COMMIT");
    // Two in a row, the far one committed first: the reader dooms the pivot.
    assert_eq!(read(&mut reader, A), i64::from(A));
    assert_eq!(
        reason(&pivot.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
    run(&mut reader, "COMMIT");
}

#[test]
fn a_reader_of_one_snail_finds_the_edge_a_writer_of_two_found_in_the_other() {
    let (p, q) = two_snails("found-there");
    let (mut pivot, mut reader, mut out) = (both(&p, &q), open(&p), open(&q));
    run_across(&mut pivot, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut reader, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read_across(&mut pivot, "s.t", 3200);
    run(&mut out, "UPDATE o.t SET v = -2 WHERE id = 32000");
    run(&mut out, "COMMIT");
    // The pivot finds its edge out in the file of what it read: it stands in the file it writes too.
    assert_eq!(read_across(&mut pivot, "o.t", B), i64::from(B));
    run_across(&mut pivot, &format!("UPDATE s.t SET v = -1 WHERE id = {A}"));
    assert_eq!(read(&mut reader, A), i64::from(A));
    assert_eq!(
        reason(&pivot.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
    run(&mut reader, "COMMIT");
}

#[test]
fn a_record_of_a_snail_only_read_goes_with_the_commit_one_root_deciding() {
    let (p, q) = two_snails("record-deciding");
    let (mut a, mut other) = (both(&p, &q), both(&p, &q));
    run_across(&mut other, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read_across(&mut other, "s.t", 3200);
    run_across(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read_across(&mut a, "s.t", A);
    run_across(&mut a, &format!("UPDATE o.t SET v = -1 WHERE id = {B}"));
    let (g, h) = (
        Snail::open(&p).unwrap().generation(),
        Snail::open(&q).unwrap().generation(),
    );
    a.stop_at(Some(warren_snail::read::StopAcross::BeforeDecision));
    assert!(a.execute("COMMIT").is_err());
    assert_eq!(
        (
            Snail::open(&p).unwrap().generation(),
            Snail::open(&q).unwrap().generation()
        ),
        (g, h)
    );
    run_across(&mut other, "ROLLBACK");
}

#[test]
fn a_session_s_level_across_snails_makes_each_statement_a_transaction() {
    let (p, q) = two_snails("session");
    let mut a = both(&p, &q);
    run_across(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_eq!(read_across(&mut a, "s.t", A), i64::from(A));
    run_across(&mut a, &format!("UPDATE o.t SET v = -1 WHERE id = {B}"));
    assert_eq!(read_across(&mut a, "o.t", B), -1);
    run_across(&mut a, "BEGIN READ ONLY");
    assert_eq!(read_across(&mut a, "o.t", B), -1);
    let e = a.execute(&format!("UPDATE o.t SET v = -2 WHERE id = {B}")).unwrap_err();
    assert_eq!(e.to_string(), "cannot execute UPDATE in a read-only transaction");
    run_across(&mut a, "ROLLBACK");
}

#[test]
fn an_older_format_among_several_refuses_the_level_by_name() {
    let (_, q) = two_snails("older-across");
    let path = tmp("serial-older-across.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            push_datum(&mut v, &warren_snail::composed::scalar_in(c.ty, "1").unwrap());
            v
        })
        .collect();
    warren_snail::write::write_format_eight(
        &path,
        &catalog,
        &[("s.t".to_string(), Rows { columns })],
        weather::span(),
    )
    .unwrap();
    let mut w = both(&path, &q);
    let e = w
        .execute("BEGIN ISOLATION LEVEL SERIALIZABLE")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("a snail of format 8 runs no SERIALIZABLE transaction"), "{e}");
}

#[test]
fn every_interleaving_of_two_transactions_across_two_snails_is_some_serial_order() {
    let (p, q) = two_snails("walk-across");
    let w = walk_on(vec![p, q], 2, &AND_INSERTS, "SERIALIZABLE", (0, 1), jobs());
    println!("{w:?}");
    assert_eq!(w.broken_count, 0, "{:#?}", w.broken);
    assert_eq!(w.schedules, 5772);
    assert!(w.committed > 0 && w.refused > 0, "{w:?}");
}

#[test]
fn at_repeatable_read_the_walk_across_two_snails_finds_histories_no_serial_order_gives() {
    let (p, q) = two_snails("walk-across-repeatable");
    let w = walk_on(vec![p, q], 2, &READS_AND_WRITES, "REPEATABLE READ", (0, 1), jobs());
    println!("{w:?}");
    assert!(w.broken_count > 0, "{w:?}");
}

/// A snail of the table, each block's rows spread so that a commit's rows cut one block in two.
fn cut_by(path: &Path) -> Done {
    let mut x = open(path);
    let values: Vec<String> = (0..1024).map(|i| format!("({}, 0)", 1 + i * 16)).collect();
    run(
        &mut x,
        &format!("INSERT INTO s.t (id, v) VALUES {}", values.join(", ")),
    )
}

fn blocks_of(path: &Path) -> Vec<u64> {
    Snail::open(path)
        .unwrap()
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.column == "id" && r.order == "t_pkey")
        .map(|r| r.rows)
        .collect()
}

#[test]
fn a_mark_follows_the_block_a_commit_at_read_committed_cut() {
    let path = made("cut");
    assert_eq!(blocks_of(&path), [1024, 1024]);
    let (mut reader, mut writer) = (open(&path), open(&path));
    let (row, heap) = (16_000, B);
    run(&mut reader, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut reader, row), i64::from(row));
    // A commit outside the level cuts the block the reader marked: the row lies in the second part.
    cut_by(&path);
    assert_eq!(blocks_of(&path), [1024, 1024, 1024]);
    run(&mut writer, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut writer, heap), i64::from(heap));
    set(&mut writer, row, -1).unwrap();
    set(&mut reader, heap, -2).unwrap();
    run(&mut writer, "COMMIT");
    assert_eq!(
        reason(&reader.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_mark_on_an_empty_order_meets_its_first_rows() {
    let path = tmp("serial-empty.snail");
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    let count = "SELECT count(*) FROM s.t t";
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(one(&a.ask(count, None).unwrap().0), 0);
    assert_eq!(one(&b.ask(count, None).unwrap().0), 0);
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (1, 1)");
    run(&mut a, "COMMIT");
    // b counted none, a's row among them; a counted none, b's among them.
    let e = b
        .execute("INSERT INTO s.t (id, v) VALUES (2, 2)")
        .unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during write."
    );
}

#[test]
fn a_condition_that_holds_no_row_marks_nothing() {
    let path = made("nothing");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    let none = "SELECT count(*) FROM s.t t WHERE t.id = 1 AND t.id = 2";
    assert_eq!(one(&a.ask(none, None).unwrap().0), 0);
    assert_eq!(one(&b.ask(none, None).unwrap().0), 0);
    set(&mut a, B, -1).unwrap();
    set(&mut b, A, -2).unwrap();
    run(&mut a, "COMMIT");
    run(&mut b, "COMMIT");
}

#[test]
fn a_read_past_the_rows_marks_the_block_its_key_belongs_with_and_a_gap_is_met() {
    let path = made("gap");
    let (mut a, mut b) = (open(&path), open(&path));
    let (gap, past) = (A + 1, 16 * ROWS + 100);
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // a looks for a key that is not there, in the first block's gap; b past the last row.
    let at = |k: i32| format!("SELECT count(*) FROM s.t t WHERE t.id = {k}");
    assert_eq!(one(&a.ask(&at(gap), None).unwrap().0), 0);
    assert_eq!(one(&b.ask(&at(past), None).unwrap().0), 0);
    run(
        &mut a,
        &format!("INSERT INTO s.t (id, v) VALUES ({past}, 1)"),
    );
    run(
        &mut b,
        &format!("INSERT INTO s.t (id, v) VALUES ({gap}, 2)"),
    );
    run(&mut a, "COMMIT");
    assert_eq!(
        reason(&b.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn past_its_marks_on_an_order_a_transaction_marks_it_whole() {
    let path = tmp("serial-whole.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows: i32 = 64 * 1024;
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..rows {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &i.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    // Every block but the last, 63 of them, read by key: the last holds no key read.
    let upto = 63 * 1024 - 1;
    let count = format!("SELECT count(*), sum(t.v) FROM s.t t WHERE t.id <= {upto}");
    a.ask(&count, None).unwrap();
    assert_eq!(read(&mut b, 0), 0);
    // A write in the last block meets a's mark on the order whole; a write in a block read meets b's.
    set(&mut b, rows - 1, -1).unwrap();
    set(&mut a, 0, -2).unwrap();
    run(&mut b, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn the_records_are_let_go_once_no_round_older_than_their_commit_runs() {
    let path = made("records");
    let size = |p: &Path| std::fs::metadata(p).unwrap().len();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut b, B);
    set(&mut b, B, -1).unwrap();
    run(&mut b, "COMMIT");
    // a's snapshot is older than b's commit: the records hold it.
    let held = Snail::open(&path).unwrap().serial_records_kept().unwrap();
    assert_eq!(held, 1);
    run(&mut a, "COMMIT");
    // A commit beside no round older lets them go.
    drop(a);
    drop(b);
    let mut c = open(&path);
    set(&mut c, A, -2).unwrap();
    assert_eq!(
        Snail::open(&path).unwrap().serial_records_kept().unwrap(),
        0
    );
    assert!(size(&path) > 0);
}

#[test]
fn a_transaction_at_serializable_reads_one_snapshot_and_its_own_writes() {
    let path = made("snapshot");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, &format!("UPDATE s.t SET v = -5 WHERE id = {A}"));
    assert_eq!(read(&mut a, A), -5);
    run(&mut b, &format!("UPDATE s.t SET v = -6 WHERE id = {A}"));
    assert_eq!(read(&mut a, A), -5);
    set(&mut a, B, -7).unwrap();
    assert_eq!(read(&mut a, B), -7);
    assert_eq!(read(&mut b, B), i64::from(B));
    run(&mut a, "COMMIT");
    assert_eq!(read(&mut a, A), -6);
    // A row another changed since is refused as at REPEATABLE READ.
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, B);
    set(&mut b, B, -8).unwrap();
    let e = set(&mut a, B, -9).unwrap_err();
    assert_eq!(
        e.to_string(),
        "could not serialize access due to concurrent update"
    );
    run(&mut a, "ROLLBACK");
    // The transaction's own statement alone, under a session's level, ends with itself.
    run(
        &mut a,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    assert_eq!(read(&mut a, B), -8);
    set(&mut a, B, -10).unwrap();
    assert_eq!(read(&mut a, B), -10);
}

#[test]
fn a_count_the_directory_answers_above_the_blocks_marks_the_order_whole() {
    let path = tmp("serial-count.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows: i32 = 256 * 1024;
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..rows {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &i.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    let (counted, round) = a.ask("SELECT count(*) FROM s.t t", None).unwrap();
    assert_eq!(one(&counted), i64::from(rows));
    assert_eq!(round.column_pages_read, 0, "the directory answered it");
    assert_eq!(read(&mut b, 5), 5);
    set(&mut b, rows / 2, -1).unwrap();
    set(&mut a, 5, -2).unwrap();
    run(&mut b, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_read_of_a_block_a_running_writer_holds_finds_its_edge() {
    let path = made("holder");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    set(&mut a, B, -1).unwrap();
    // b reads what a holds uncommitted: the edge is the read's to find, a having written first.
    assert_eq!(read(&mut b, B), i64::from(B));
    set(&mut b, A, -2).unwrap();
    run(&mut a, "COMMIT");
    assert_eq!(
        reason(&b.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

/// The three that make a pivot: 0 read what 1 writes, 2 read what 0 writes.
fn pivot_of_three(path: &Path) -> (Snail, Snail, Snail) {
    let (mut p, mut out, mut inn) = (open(path), open(path), open(path));
    for s in [&mut p, &mut out, &mut inn] {
        run(s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    }
    read(&mut p, A);
    set(&mut out, A, -2).unwrap();
    read(&mut inn, B);
    set(&mut p, B, -1).unwrap();
    (p, out, inn)
}

#[test]
fn each_failure_comes_where_postgresql_s_monitor_fires_it_in_its_words() {
    // A commit dooms the pivot beside its other edge; the pivot fails at its next read.
    let path = made("reasons-read");
    let (mut p, mut out, mut inn) = pivot_of_three(&path);
    run(&mut out, "COMMIT");
    let sql = format!("SELECT sum(t.v) FROM s.t t WHERE t.id = {A}");
    assert_eq!(
        reason(&p.ask(&sql, None).map(|_| ()).unwrap_err()),
        "Canceled on identification as a pivot, during conflict out checking."
    );
    assert!(p.ask(&sql, None).is_err());
    run(&mut p, "ROLLBACK");
    run(&mut inn, "COMMIT");
    // At its next write.
    let path = made("reasons-write");
    let (mut p, mut out, mut inn) = pivot_of_three(&path);
    run(&mut out, "COMMIT");
    let e = p
        .execute(&format!("INSERT INTO s.t (id, v) VALUES ({}, 3)", B + 3))
        .unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during conflict in checking."
    );
    run(&mut p, "ROLLBACK");
    run(&mut inn, "COMMIT");
    // At its commit; and the transaction that read it goes on.
    let path = made("reasons-commit");
    let (mut p, mut out, mut inn) = pivot_of_three(&path);
    run(&mut out, "COMMIT");
    run(&mut inn, "COMMIT");
    assert_eq!(
        reason(&p.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
    // A doom outlives its doomer's rollback: a read beside a running pivot dooms it, one commit out.
    let path = made("reasons-rollback");
    let (mut p, mut out, mut inn) = (open(&path), open(&path), open(&path));
    for s in [&mut p, &mut out, &mut inn] {
        run(s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    }
    read(&mut inn, A);
    read(&mut p, A);
    set(&mut out, A, -2).unwrap();
    set(&mut p, B, -1).unwrap();
    run(&mut out, "COMMIT");
    read(&mut inn, B);
    run(&mut inn, "ROLLBACK");
    assert_eq!(
        reason(&p.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
    // A read meeting a pivot that committed reading over a commit before it is refused itself.
    let path = made("reasons-conflict-out");
    let (mut p, mut out, mut inn) = (open(&path), open(&path), open(&path));
    for s in [&mut p, &mut out, &mut inn] {
        run(s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    }
    read(&mut inn, A);
    read(&mut p, A);
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    set(&mut p, B, -1).unwrap();
    run(&mut p, "COMMIT");
    let sql = format!("SELECT sum(t.v) FROM s.t t WHERE t.id = {B}");
    let said = reason(&inn.ask(&sql, None).map(|_| ()).unwrap_err());
    assert!(
        said.starts_with("Canceled on conflict out to pivot ") && said.ends_with(", during read."),
        "{said}"
    );
    run(&mut inn, "ROLLBACK");
}

#[test]
fn a_reader_read_only_before_the_commit_it_would_need_stands_and_the_pivot_too() {
    let path = made("before");
    let (mut p, mut out, mut ro) = (open(&path), open(&path), open(&path));
    run(&mut p, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut p, A);
    set(&mut p, B, -1).unwrap();
    run(&mut ro, "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY");
    read(&mut ro, A);
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    assert_eq!(read(&mut ro, B), i64::from(B));
    run(&mut ro, "COMMIT");
    run(&mut p, "COMMIT");
}

#[test]
fn deferrable_beside_a_writer_that_read_over_a_commit_takes_a_new_snapshot() {
    let path = made("unsafe");
    let (mut p, mut out) = (open(&path), open(&path));
    run(&mut p, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut p, A);
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    let q = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut d = open(&q);
        d.execute("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE")
            .unwrap();
        tx.send(read(&mut d, B)).unwrap();
        d.execute("COMMIT").unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    // p commits having read over out's commit, which came before the first snapshot: it is unsafe,
    // and a new one, with p's commit in it, is taken.
    set(&mut p, B, -1).unwrap();
    run(&mut p, "COMMIT");
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), -1);
    t.join().unwrap();
}

#[test]
fn read_only_found_safe_beside_writers_that_ended_marks_nothing_more() {
    let path = made("safe");
    let (mut w, mut ro, mut x) = (open(&path), open(&path), open(&path));
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, A);
    run(&mut ro, "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY");
    read(&mut ro, A);
    run(&mut w, "COMMIT");
    // The writer beside it ended with no edge out: the reader is safe, and a later read marks nothing,
    // nor does a writer of what it read meet it.
    read(&mut ro, B);
    run(&mut x, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut x, B);
    set(&mut x, A, -1).unwrap();
    run(&mut x, "COMMIT");
    assert_eq!(
        Snail::open(&path).unwrap().serial_records_kept().unwrap(),
        1
    );
    // Beside another writer open, it writes no records at its commit.
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, A);
    let generation = warren_snail::carry::newest(&path).unwrap();
    run(&mut ro, "COMMIT");
    assert_eq!(warren_snail::carry::newest(&path).unwrap(), generation);
    run(&mut w, "COMMIT");
}

#[test]
fn a_statement_that_waited_for_a_commit_that_doomed_it_is_refused_as_concurrent_update() {
    let path = made("waited");
    let mut a = open(&path);
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut a, A);
    set(&mut a, B, -1).unwrap();
    let q = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut b = open(&q);
        b.execute("BEGIN ISOLATION LEVEL SERIALIZABLE").unwrap();
        set(&mut b, A, -2).unwrap();
        tx.send(set(&mut b, B, -3).map(|_| ()).map_err(|e| e.to_string()))
            .unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    run(&mut a, "COMMIT");
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(10))
            .unwrap()
            .unwrap_err(),
        "could not serialize access due to concurrent update"
    );
    t.join().unwrap();
}

/// Each side of write skew reads by `reading`, the rows it reads and the row the other writes the
/// same.
fn skew_read_by(name: &str, reading: fn(&mut Snail, i32)) {
    let path = made(name);
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    reading(&mut a, A);
    reading(&mut b, B);
    set(&mut a, B, -1).unwrap();
    set(&mut b, A, -2).unwrap();
    run(&mut a, "COMMIT");
    assert_eq!(
        reason(&b.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_join_s_reads_are_marked() {
    skew_read_by("join", |s, x| {
        s.ask(
            &format!("SELECT count(*) FROM s.t p JOIN s.t q ON p.id = q.id WHERE p.id = {x}"),
            None,
        )
        .unwrap();
    });
}

#[test]
fn a_round_handed_out_a_block_at_a_time_is_marked() {
    skew_read_by("each", |s, x| {
        s.ask_each(
            &format!("SELECT t.v FROM s.t t WHERE t.id = {x}"),
            None,
            &mut |_| Ok(()),
        )
        .unwrap();
    });
}

#[test]
fn a_pipeline_s_reads_are_marked() {
    skew_read_by("pipeline", |s, x| {
        s.ask(
            &format!("WITH q AS (SELECT t.v FROM s.t t WHERE t.id = {x}) SELECT q.v FROM q"),
            None,
        )
        .unwrap();
    });
}

#[test]
fn a_snapshot_taken_after_a_commit_meets_nothing_of_it() {
    let path = made("after");
    let (mut w, mut out, mut later) = (open(&path), open(&path), open(&path));
    // w commits having read over out's commit, its edge out to a commit before its own.
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, A);
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    set(&mut w, B, -1).unwrap();
    run(&mut w, "COMMIT");
    // A transaction after them both reads what w wrote: no edge at all.
    run(&mut later, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut later, B), -1);
    run(&mut later, "COMMIT");
}

#[test]
fn a_read_of_a_writer_whose_edge_out_came_after_its_commit_stands() {
    let path = made("order");
    let (mut w, mut t2, mut r) = (open(&path), open(&path), open(&path));
    for s in [&mut w, &mut t2, &mut r] {
        run(s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    }
    read(&mut r, 3200);
    read(&mut w, A);
    set(&mut w, B, -1).unwrap();
    run(&mut w, "COMMIT");
    // t2 commits after w: w's edge out reaches a commit after its own, so r, reading w, stands.
    set(&mut t2, A, -2).unwrap();
    run(&mut t2, "COMMIT");
    assert_eq!(read(&mut r, B), i64::from(B));
    run(&mut r, "COMMIT");
}

#[test]
fn a_reader_with_an_edge_in_that_reads_a_commit_it_cannot_see_is_refused_there() {
    let path = made("reader-pivot");
    let (mut r, mut t0, mut w) = (open(&path), open(&path), open(&path));
    for s in [&mut r, &mut t0, &mut w] {
        run(s, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    }
    set(&mut r, A, -1).unwrap();
    // t0 reads what r wrote and cannot see; w writes what r reads next.
    assert_eq!(read(&mut t0, A), i64::from(A));
    set(&mut w, B, -2).unwrap();
    run(&mut w, "COMMIT");
    let sql = format!("SELECT sum(t.v) FROM s.t t WHERE t.id = {B}");
    let said = reason(&r.ask(&sql, None).map(|_| ()).unwrap_err());
    assert!(
        said.starts_with("Canceled on conflict out to pivot ") && said.ends_with(", during read."),
        "{said}"
    );
    run(&mut r, "ROLLBACK");
    run(&mut t0, "COMMIT");
}

#[test]
fn a_commit_dooms_no_pivot_whose_other_edge_comes_from_read_only() {
    let path = made("far-read-only");
    let (mut p, mut out, mut ro) = (open(&path), open(&path), open(&path));
    run(&mut p, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut p, A);
    set(&mut p, B, -1).unwrap();
    run(&mut ro, "BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY");
    assert_eq!(read(&mut ro, B), i64::from(B));
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    run(&mut ro, "COMMIT");
    run(&mut p, "COMMIT");
}

#[test]
fn a_read_of_a_block_as_it_was_before_a_cut_meets_a_write_of_a_block_cut_from_it() {
    let path = made("read-parent");
    let (mut a, mut w) = (open(&path), open(&path));
    let row = 16_000;
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut a, B), i64::from(B));
    // A commit outside the level cuts the first block; a writer at the level writes a part of it and
    // reads what a writes.
    cut_by(&path);
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, 32016);
    set(&mut w, row, -1).unwrap();
    run(&mut w, "COMMIT");
    // a reads the block as its snapshot holds it, whole: it did not see w's write, and writes what w
    // read.
    assert_eq!(read(&mut a, row), i64::from(row));
    let e = set(&mut a, 32016, -2).unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during write."
    );
}

#[test]
fn a_read_of_a_block_before_a_cut_meets_a_running_writer_of_a_part_of_it() {
    let path = made("read-part");
    let (mut a, mut w) = (open(&path), open(&path));
    let row = 16_000;
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut a, B), i64::from(B));
    cut_by(&path);
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut w, 32016);
    set(&mut w, row, -1).unwrap();
    // a reads the block whole, as its snapshot holds it; a part of it is w's, still running.
    assert_eq!(read(&mut a, row), i64::from(row));
    set(&mut a, 32016, -2).unwrap();
    run(&mut w, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_read_of_a_block_whose_first_key_its_own_rows_lowered_marks_the_block_its_snapshot_names() {
    let path = made("lowered");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut a, "INSERT INTO s.t (id, v) VALUES (-5, 1)");
    assert_eq!(read(&mut a, 0), 0);
    assert_eq!(read(&mut b, B), i64::from(B));
    set(&mut b, 16, -1).unwrap();
    set(&mut a, B, -2).unwrap();
    run(&mut b, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_statement_alone_at_the_level_meets_the_reader_of_its_block() {
    let path = made("alone");
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut a, A), i64::from(A));
    run(
        &mut b,
        "SET SESSION CHARACTERISTICS AS TRANSACTION ISOLATION LEVEL SERIALIZABLE",
    );
    set(&mut b, A, -1).unwrap();
    // a read what b wrote, and b, committed first, read what a writes.
    let e = set(&mut a, 3200, -2).unwrap_err();
    assert_eq!(
        reason(&e),
        "Canceled on identification as a pivot, during write."
    );
}

#[test]
fn a_read_of_an_order_whole_after_a_write_finds_its_writer() {
    let path = tmp("serial-whole-after.snail");
    let catalog = Catalog::parse(DDL).unwrap();
    let table = catalog.table("s.t").unwrap();
    let rows: i32 = 256 * 1024;
    let columns = table
        .columns
        .iter()
        .map(|c| {
            let mut v = Values::new(c.ty);
            for i in 0..rows {
                push_datum(
                    &mut v,
                    &warren_snail::composed::scalar_in(c.ty, &i.to_string()).unwrap(),
                );
            }
            v
        })
        .collect();
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    let (mut a, mut b) = (open(&path), open(&path));
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    run(&mut b, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut b, 5);
    set(&mut b, rows / 2, -1).unwrap();
    let (_, round) = a.ask("SELECT count(*) FROM s.t t", None).unwrap();
    assert_eq!(round.column_pages_read, 0, "the directory answered it");
    set(&mut a, 5, -2).unwrap();
    run(&mut b, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn a_block_a_prune_lets_go_leaves_its_mark_on_the_order() {
    let path = made("gone");
    let (mut a, mut w) = (open(&path), open(&path));
    let row = 16_000;
    run(
        &mut w,
        "CREATE TABLE s.o (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id))",
    );
    run(&mut w, "INSERT INTO s.o (id, v) VALUES (1, 1)");
    run(&mut a, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    assert_eq!(read(&mut a, row), i64::from(row));
    // Outside the level, every row of the first block goes, and the block with them.
    let mut x = open(&path);
    run(&mut x, "DELETE FROM s.t WHERE id < 16384");
    x.ask("SELECT sum(t.v) FROM s.t t", None).unwrap();
    assert_eq!(
        blocks_of(&path),
        [1024],
        "the read pruned the first block away"
    );
    // A row written where the first block's rows lay lies in the block that remains.
    run(&mut w, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    w.ask("SELECT sum(o.v) FROM s.o o WHERE o.id = 1", None)
        .unwrap();
    run(
        &mut w,
        &format!("INSERT INTO s.t (id, v) VALUES ({row}, 1)"),
    );
    run(&mut a, "UPDATE s.o SET v = -2 WHERE id = 1");
    run(&mut w, "COMMIT");
    assert_eq!(
        reason(&a.execute("COMMIT").unwrap_err()),
        "Canceled on identification as a pivot, during commit attempt."
    );
}

#[test]
fn deferrable_takes_its_new_snapshot_as_soon_as_one_shows_it_unsafe() {
    let path = made("unsafe-at-once");
    let (mut p, mut out, mut y) = (open(&path), open(&path), open(&path));
    run(
        &mut y,
        "CREATE TABLE s.o (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id))",
    );
    run(&mut y, "INSERT INTO s.o (id, v) VALUES (1, 1)");
    run(&mut p, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    read(&mut p, A);
    run(&mut y, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    y.ask("SELECT sum(o.v) FROM s.o o WHERE o.id = 1", None)
        .unwrap();
    run(&mut out, "BEGIN ISOLATION LEVEL SERIALIZABLE");
    set(&mut out, A, -2).unwrap();
    run(&mut out, "COMMIT");
    let q = path.clone();
    let (tx, rx) = std::sync::mpsc::channel();
    let t = std::thread::spawn(move || {
        let mut d = open(&q);
        d.execute("BEGIN ISOLATION LEVEL SERIALIZABLE READ ONLY DEFERRABLE")
            .unwrap();
        let (b, _) = d
            .ask("SELECT sum(o.v) FROM s.o o WHERE o.id = 1", None)
            .unwrap();
        tx.send(one(&b)).unwrap();
        d.execute("COMMIT").unwrap();
    });
    assert!(rx.recv_timeout(Duration::from_millis(300)).is_err());
    // p shows the first snapshot unsafe: the new one is taken at once, y still open beside it.
    set(&mut p, B, -1).unwrap();
    run(&mut p, "COMMIT");
    std::thread::sleep(Duration::from_millis(300));
    run(&mut y, "UPDATE s.o SET v = 7 WHERE id = 1");
    run(&mut y, "COMMIT");
    // It waited for y's end all the same, and reads as of the snapshot before it.
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), 1);
    t.join().unwrap();
}
