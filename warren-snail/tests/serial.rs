//! SERIALIZABLE as PostgreSQL has it: a transaction's reads marked on each block, the edges between
//! overlapping transactions found from both sides, and two in a row refused where PostgreSQL's
//! monitor refuses them, in its words; READ ONLY and DEFERRABLE; the records a commit leaves; the
//! older formats and a replica refused by name; and every interleaving of two transactions over two
//! blocks held to some serial order.
#![cfg(feature = "standalone")]

mod weather;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::time::Duration;
use warren_snail::catalog::Catalog;
use warren_snail::read::{Done, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{one, tmp, write};

const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, v integer NOT NULL, PRIMARY KEY (id));";
/// The table's rows, every sixteenth id: two blocks of 1,024, so `A` lies in the first and `B` in
/// the second, and a row inserted beside either lies in its block.
const ROWS: i32 = 2048;
const A: i32 = 1600;
const B: i32 = 32000;

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
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), i64::from(B));
    t.join().unwrap();
    // DEFERRABLE is set before any query, and a transaction not READ ONLY takes it and waits for
    // nothing.
    run(&mut w, "BEGIN");
    read(&mut w, A);
    assert_eq!(
        w.execute("SET TRANSACTION DEFERRABLE").unwrap_err().to_string(),
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
    run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
    let e = a
        .execute("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("cannot use serializable mode in a hot standby"), "{e}");
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
        let other = |x: i32| if x == A { B } else { A };
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
    match x == A {
        true => (0, 16_383),
        false => (16_384, 16 * ROWS),
    }
}

fn statement(step: Step, t: usize, k: usize) -> String {
    match step {
        Step::Read(x) => {
            let (a, z) = range_of(x);
            format!("SELECT sum(t.v), count(*) FROM s.t t WHERE t.id >= {a} AND t.id <= {z}")
        }
        Step::Write(x) => format!("UPDATE s.t SET v = {} WHERE id = {x}", value_of(t, k)),
        Step::Insert(x) => format!(
            "INSERT INTO s.t (id, v) VALUES ({}, {})",
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

/// What one step of a transaction came to: its read's answer, or none, or the words of its failure.
type Came = Result<Option<(i64, i64)>, String>;

/// A transaction on a thread of its own: statements in, outcomes out.
fn transaction(path: PathBuf) -> (Sender<String>, Receiver<Came>) {
    let (to, rx) = channel::<String>();
    let (tx, from) = channel();
    std::thread::spawn(move || {
        let mut s = Snail::open_writing(&path).unwrap();
        for st in rx {
            let came = if st.starts_with("SELECT") {
                s.ask(&st, None)
                    .map(|(b, _)| {
                        let column = |c: usize| {
                            let a = b.column(c);
                            let a = a
                                .as_any()
                                .downcast_ref::<arrow::array::Int64Array>()
                                .expect("a sum or a count");
                            arrow::array::Array::is_valid(a, 0).then(|| a.value(0)).unwrap_or(0)
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
    base: &Path,
    at: &Path,
    transactions: &[Vec<Step>],
    order: &[u8],
    level: &str,
) -> (Vec<(bool, Vec<Option<(i64, i64)>>)>, [(i64, i64); 2]) {
    std::fs::copy(base, at).unwrap();
    let seen = std::fs::File::open(at).unwrap();
    let n = transactions.len();
    let sessions: Vec<_> = (0..n).map(|_| transaction(at.to_path_buf())).collect();
    let mut answers: Vec<Vec<Option<(i64, i64)>>> = vec![Vec::new(); n];
    let mut failed = vec![false; n];
    let mut committed = vec![false; n];
    let mut next = vec![0; n];
    let mut pending: Vec<Vec<usize>> = vec![Vec::new(); n];
    for s in &sessions {
        s.0.send(format!("BEGIN ISOLATION LEVEL {level}")).unwrap();
        assert!(s.1.recv().unwrap().is_ok());
    }
    let settle = |pending: &mut Vec<Vec<usize>>,
                      answers: &mut Vec<Vec<Option<(i64, i64)>>>,
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
            if running == 0 || (!to_the_end && running == warren_snail::lock::waiting(&seen).unwrap())
            {
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
            Some(step) => statement(*step, t, k),
            None => "COMMIT".to_string(),
        };
        pending[t].push(k);
        sessions[t].0.send(st).unwrap();
        settle(&mut pending, &mut answers, &mut failed, &mut committed, false);
    }
    settle(&mut pending, &mut answers, &mut failed, &mut committed, true);
    drop(sessions);
    let mut after = Snail::open(at).unwrap();
    let mut block = |x: i32| {
        let (a, z) = range_of(x);
        let (b, _) = after
            .ask(
                &format!(
                    "SELECT sum(t.v), count(*) FROM s.t t WHERE t.id >= {a} AND t.id <= {z}"
                ),
                None,
            )
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
fn some_serial_order(
    transactions: &[Vec<Step>],
    ran: &[(bool, Vec<Option<(i64, i64)>>)],
    rows: [(i64, i64); 2],
) -> bool {
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
    fn go(all: &[Vec<Step>], from: usize, n: usize, now: &mut Vec<usize>, out: &mut Vec<Vec<usize>>) {
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
fn walk(n: usize, statements: &[Step], level: &str, (shard, of): (usize, usize), jobs: usize) -> Walked {
    let base = tmp(&format!("serial-walk-{n}-{}.snail", level.replace(' ', "-")));
    {
        let made = made(&format!("walk-base-{n}-{}", level.replace(' ', "-")));
        std::fs::copy(&made, &base).unwrap();
    }
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
            let (work, at, found, base, dir) = (&work, &at, &found, &base, &dir);
            scope.spawn(move || {
                let file = dir.join(format!(
                    "warren-snail-serial-{}-{n}-{job}-{}.snail",
                    std::process::id(),
                    level.replace(' ', "-")
                ));
                loop {
                    let i = at.fetch_add(1, Ordering::Relaxed);
                    let Some((set, order)) = work.get(i) else {
                        break;
                    };
                    let (ran, rows) = run_interleaving(base, &file, set, order, level);
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
                let _ = std::fs::remove_file(&file);
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

/// The certificate's own walk, `serial.nu`: three transactions, at SERIALIZABLE.
#[test]
#[ignore]
fn every_interleaving_of_three_transactions_over_two_blocks_is_some_serial_order() {
    let w = walk(3, &READS_AND_WRITES, "SERIALIZABLE", shard(), jobs());
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
