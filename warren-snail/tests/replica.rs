//! A snail marked a replica of a named source: the mark lies in the root and is set and cleared by a
//! commit of its own, or of the transaction under way; while it stands, every writer is refused by
//! name but the one presenting itself as that source's applier, at its statement and at its commit,
//! a stand-alone writer and one inside a host alike; a read is pruned by the applier's reads alone;
//! and a follower that is its leader byte for byte carries the leader's mark, refusing a stray writer
//! while the leader's commits land.

mod weather;

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use warren_snail::carry::{Commit, Host, Judge};
use warren_snail::read::Snail;
use weather::*;

const SOURCE: &str = "mysql-primary 3306";

fn station(no: i64, name: &str) -> String {
    format!(
        "INSERT INTO weather.stations (network_id, station_no, name) VALUES (9001, {no}, '{name}')"
    )
}

#[cfg(feature = "standalone")]
fn named(path: &Path, name: &str) -> i64 {
    let mut s = Snail::open(path).unwrap();
    one(&s
        .ask(
            &format!("SELECT count(*) FROM weather.stations s WHERE s.name = '{name}'"),
            None,
        )
        .unwrap()
        .0)
}

fn refused(e: warren_snail::Error) -> bool {
    let e = e.to_string();
    e.contains(&format!("is marked a replica of '{SOURCE}'"))
        && e.contains(&format!("only the applier presenting '{SOURCE}' writes it"))
}

/// A host that tags each transaction's root with `tag` and keeps every commit it carries.
struct Tagging {
    tag: u64,
    log: Arc<Mutex<Vec<Commit>>>,
}

impl Host for Tagging {
    fn carry(&mut self, commit: &Commit) -> warren_snail::Result<()> {
        self.log.lock().unwrap().push(commit.clone());
        Ok(())
    }

    fn tag(&self) -> Option<u64> {
        Some(self.tag)
    }
}

/// A judge that takes the tags its host says committed.
#[derive(Clone, Default)]
struct Said(Rc<RefCell<Vec<u64>>>);

impl Judge for Said {
    fn taken(&self, tag: u64) -> warren_snail::Result<bool> {
        Ok(self.0.borrow().contains(&tag))
    }
}

fn hosted(path: &Path, tag: u64, said: &Said) -> (Snail, Arc<Mutex<Vec<Commit>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let host = Tagging {
        tag,
        log: log.clone(),
    };
    let w = Snail::open_carried(path, Box::new(host), Rc::new(said.clone())).unwrap();
    (w, log)
}

#[test]
fn inside_a_host_the_mark_is_the_transactions_commit_and_its_tagged_root_carries_it() {
    let s = sample(2_000);
    let path = snail("replica-hosted", &s);
    let g = Snail::open(&path).unwrap().generation();
    let said = Said::default();
    let (mut w, log) = hosted(&path, 7, &said);
    w.execute("BEGIN").unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    // Within the transaction, its own writes after the mark are a writer's like any other's.
    assert!(refused(w.execute(&station(1, "Sagres 1")).unwrap_err()));
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&station(1, "Sagres 1")).unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    w.execute("COMMIT").unwrap();
    drop(w);
    let carried = log.lock().unwrap().clone();
    assert_eq!(carried.len(), 1);
    assert_eq!(
        (
            carried[0].generation,
            carried[0].host(),
            carried[0].replica()
        ),
        (g + 1, Some(7), Some(SOURCE.to_string()))
    );
    // A round that does not take the tagged root reads the parent, no replica.
    assert_eq!(Snail::open(&path).unwrap().replica(), None);
    said.0.borrow_mut().push(7);
    let judged = Snail::open_judged(&path, Rc::new(said.clone())).unwrap();
    assert_eq!(
        (judged.generation(), judged.replica()),
        (g + 1, Some(SOURCE))
    );
    // Another writer of the host is refused by name, at its statement; the applier writes, and its
    // root carries the mark on.
    let (mut other, log) = hosted(&path, 8, &said);
    assert!(refused(other.execute(&station(2, "Sagres 2")).unwrap_err()));
    drop(other);
    assert!(log.lock().unwrap().is_empty());
    let (mut applier, log) = hosted(&path, 9, &said);
    applier.applying(SOURCE).unwrap();
    applier.execute(&station(3, "Sagres 3")).unwrap();
    drop(applier);
    let c = log.lock().unwrap()[0].clone();
    assert_eq!(
        (c.generation, c.host(), c.replica()),
        (g + 2, Some(9), Some(SOURCE.to_string()))
    );
    // Cleared by a writer that is no applier, as when the applier is gone for good: then it writes.
    said.0.borrow_mut().push(9);
    let (mut w, log) = hosted(&path, 10, &said);
    w.execute("BEGIN").unwrap();
    w.mark_replica(None).unwrap();
    w.execute(&station(4, "Sagres 4")).unwrap();
    w.execute("COMMIT").unwrap();
    drop(w);
    let c = log.lock().unwrap()[0].clone();
    assert_eq!((c.generation, c.replica()), (g + 3, None));
    // A snail opened to read presents no applier and marks nothing.
    let mut reader = Snail::open(&path).unwrap();
    assert!(reader.applying(SOURCE).is_err());
    assert!(reader.mark_replica(Some(SOURCE)).is_err());
}

#[test]
#[cfg(feature = "standalone")]
fn a_replica_takes_writes_from_its_sources_applier_alone() {
    let s = sample(2_000);
    let path = snail("replica-applier", &s);
    let g = Snail::open(&path).unwrap().generation();
    // A writer opened before the mark, and the mark a commit of its own.
    let mut before = Snail::open_writing(&path).unwrap();
    let mut marker = Snail::open_writing(&path).unwrap();
    let done = marker.mark_replica(Some(SOURCE)).unwrap();
    assert_eq!(done.generation, g + 1);
    assert!(done.pages_written > 0);
    assert_eq!(Snail::open(&path).unwrap().replica(), Some(SOURCE));
    // Every writer but the source's applier is refused by name: one opened before the mark, the
    // writer that set it, one opened after it, and the applier of another source.
    assert!(refused(
        before.execute(&station(1, "Sagres 1")).unwrap_err()
    ));
    assert!(refused(
        marker.execute(&station(1, "Sagres 1")).unwrap_err()
    ));
    let mut after = Snail::open_writing(&path).unwrap();
    assert!(refused(after.execute(&station(1, "Sagres 1")).unwrap_err()));
    let mut other = Snail::open_writing(&path).unwrap();
    other.applying("mysql-other").unwrap();
    assert!(refused(other.execute(&station(1, "Sagres 1")).unwrap_err()));
    assert!(refused(
        after
            .execute("DELETE FROM weather.stations WHERE network_id = 9001")
            .unwrap_err()
    ));
    assert_eq!(Snail::open(&path).unwrap().generation(), g + 1);
    // The applier writes, and every commit of its carries the mark on.
    let mut applier = Snail::open_writing(&path).unwrap();
    applier.applying(SOURCE).unwrap();
    applier.execute(&station(1, "Sagres 1")).unwrap();
    applier.execute("BEGIN").unwrap();
    applier.execute(&station(2, "Sagres 2")).unwrap();
    applier
        .execute("UPDATE weather.stations SET name = 'Sagres 2b' WHERE network_id = 9001 AND station_no = 2")
        .unwrap();
    applier.execute("COMMIT").unwrap();
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 3, Some(SOURCE)));
    assert_eq!(
        (named(&path, "Sagres 1"), named(&path, "Sagres 2b")),
        (1, 1)
    );
    // Marking it as it is marked writes nothing; another source's mark is refused until cleared.
    let done = marker.mark_replica(Some(SOURCE)).unwrap();
    assert_eq!((done.generation, done.pages_written), (g + 3, 0));
    let e = marker
        .mark_replica(Some("mysql-other"))
        .unwrap_err()
        .to_string();
    assert!(
        e.contains(&format!(
            "is marked a replica of '{SOURCE}': clear the mark"
        )),
        "{e}"
    );
    assert!(marker.mark_replica(Some("")).is_err());
    // Cleared, at cutover: every writer writes again, and clearing it again writes nothing.
    assert_eq!(marker.mark_replica(None).unwrap().generation, g + 4);
    assert_eq!(Snail::open(&path).unwrap().replica(), None);
    before.execute(&station(3, "Sagres 3")).unwrap();
    assert_eq!(marker.mark_replica(None).unwrap().pages_written, 0);
    assert_eq!(Snail::open(&path).unwrap().generation(), g + 5);
    // Marked again, a writer's transaction begun before it is refused at its commit, at the root's
    // turn, and the file stands at the mark.
    let mut early = Snail::open_writing(&path).unwrap();
    early.execute("BEGIN").unwrap();
    early.execute(&station(4, "Sagres 4")).unwrap();
    marker.mark_replica(Some(SOURCE)).unwrap();
    assert!(refused(early.execute("COMMIT").unwrap_err()));
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 6, Some(SOURCE)));
    assert_eq!(named(&path, "Sagres 4"), 0);
}

#[test]
#[cfg(feature = "standalone")]
fn a_transaction_marks_the_replica_at_its_commit_and_a_rollback_leaves_the_mark_as_it_stood() {
    let s = sample(2_000);
    let path = snail("replica-transaction", &s);
    let g = Snail::open(&path).unwrap().generation();
    let mut w = Snail::open_writing(&path).unwrap();
    let e = w.mark_replica(Some("")).unwrap_err().to_string();
    assert!(e.contains("the name is empty"), "{e}");
    w.execute("BEGIN").unwrap();
    w.execute(&station(1, "Sagres 1")).unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    assert_eq!(Snail::open(&path).unwrap().replica(), None);
    // Within the transaction, another source's mark is refused as it is asked.
    let e = w.mark_replica(Some("mysql-other")).unwrap_err().to_string();
    assert!(e.contains("clear the mark"), "{e}");
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&station(1, "Sagres 1")).unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    w.execute("COMMIT").unwrap();
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 1, Some(SOURCE)));
    assert_eq!(named(&path, "Sagres 1"), 1);
    // A clear rolled back leaves the mark; a clear in the transaction lets its writes after it in.
    w.execute("BEGIN").unwrap();
    w.mark_replica(None).unwrap();
    w.execute("ROLLBACK").unwrap();
    assert_eq!(Snail::open(&path).unwrap().replica(), Some(SOURCE));
    assert!(refused(w.execute(&station(2, "Sagres 2")).unwrap_err()));
    w.execute("BEGIN").unwrap();
    w.mark_replica(None).unwrap();
    w.execute(&station(2, "Sagres 2")).unwrap();
    w.execute("COMMIT").unwrap();
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 2, None));
    assert_eq!(named(&path, "Sagres 2"), 1);
    // A transaction marking the snail, another source's mark landing first: refused at its COMMIT,
    // at the root's turn.
    let mut other = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    other.mark_replica(Some("mysql-other")).unwrap();
    let e = w.execute("COMMIT").unwrap_err().to_string();
    assert!(
        e.contains("is marked a replica of 'mysql-other': clear the mark"),
        "{e}"
    );
    assert_eq!(Snail::open(&path).unwrap().replica(), Some("mysql-other"));
    other.mark_replica(None).unwrap();
    // A transaction refused by the mark takes nothing but its end.
    w.mark_replica(Some(SOURCE)).unwrap();
    w.execute("BEGIN").unwrap();
    assert!(refused(w.execute(&station(3, "Sagres 3")).unwrap_err()));
    assert!(w.mark_replica(None).is_err());
    w.execute("ROLLBACK").unwrap();
    assert_eq!(Snail::open(&path).unwrap().replica(), Some(SOURCE));
}

#[test]
#[cfg(feature = "standalone")]
fn a_sequences_advance_lands_ahead_of_the_transaction_that_marks_the_replica() {
    use warren_snail::catalog::Catalog;
    let path = tmp("replica-sequence.snail");
    let catalog = Catalog::parse(
        "CREATE SEQUENCE s.ids START 100;
         CREATE TABLE s.q (id bigint NOT NULL DEFAULT nextval('s.ids'), v integer, PRIMARY KEY (id));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let g = Snail::open(&path).unwrap().generation();
    let mut w = Snail::open_writing(&path).unwrap();
    w.applying(SOURCE).unwrap();
    w.execute("BEGIN").unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    w.execute("INSERT INTO s.q (v) VALUES (1)").unwrap();
    // The advance landed at once, unmarked; rolled back, the mark never lands.
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 1, None));
    w.execute("ROLLBACK").unwrap();
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 1, None));
    // Marked, the applier's advances land as its writes do, and another writer's are refused.
    w.mark_replica(Some(SOURCE)).unwrap();
    w.execute("INSERT INTO s.q (v) VALUES (2)").unwrap();
    let mut other = Snail::open_writing(&path).unwrap();
    other.execute("BEGIN").unwrap();
    assert!(refused(
        other.execute("INSERT INTO s.q (v) VALUES (3)").unwrap_err()
    ));
}

/// A table of 3,000 rows on (a, b), a always 1 and b counting from 0.
#[cfg(feature = "standalone")]
fn counted(name: &str) -> std::path::PathBuf {
    use warren_snail::catalog::Catalog;
    use warren_snail::values::Values;
    use warren_snail::write::Rows;
    let path = tmp(name);
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (a integer NOT NULL, b bigint NOT NULL);
         CREATE INDEX o ON s.t (a, b);
         ALTER TABLE s.t CLUSTER ON o;",
    )
    .unwrap();
    let rows = Rows {
        columns: vec![
            Values::Int((0..3000).map(|_| Some(1)).collect()),
            Values::Int((0..3000).map(Some).collect()),
        ],
    };
    write(&path, &catalog, &[("s.t".into(), rows)]).unwrap();
    path
}

#[test]
#[cfg(feature = "standalone")]
fn a_replica_is_pruned_by_its_appliers_reads_alone_and_goes_out_dead_rows_and_all_by_no_other() {
    // The second block holds 1,976 rows over 5 pages of b; 396 dead waste a page of a read of it
    // (`blocks.rs`).
    let path = counted("replica-prune.snail");
    let q = "SELECT t.b FROM s.t t WHERE t.a = 1 AND t.b >= 1024";
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("DELETE FROM s.t WHERE a = 1 AND b >= 1024 AND b < 1420")
        .unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    let g = Snail::open(&path).unwrap().generation();
    let (got, round) = w.ask(q, None).unwrap();
    assert_eq!((got.num_rows(), round.blocks_pruned), (1976 - 396, 0));
    assert_eq!(Snail::open(&path).unwrap().generation(), g);
    let dir = tmp("replica-prune-out");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let e = w.out(&dir).unwrap_err().to_string();
    assert!(
        e.contains("blocks hold dead rows") && refused(warren_snail::Error::new(e.clone())),
        "{e}"
    );
    let mut applier = Snail::open_writing(&path).unwrap();
    applier.applying(SOURCE).unwrap();
    let (got, round) = applier.ask(q, None).unwrap();
    assert_eq!((got.num_rows(), round.blocks_pruned), (1976 - 396, 1));
    let now = Snail::open(&path).unwrap();
    assert_eq!((now.generation(), now.replica()), (g + 1, Some(SOURCE)));
    // Pruned, its blocks go out by any writer, and a recut carries the mark.
    let mut x = Snail::open_writing(&path).unwrap();
    x.out(&dir).unwrap();
    let to = tmp("replica-prune-recut.snail");
    x.recut(&to).unwrap();
    assert_eq!(Snail::open(&to).unwrap().replica(), Some(SOURCE));
}

/// A host that sends each commit on, a majority holding it once sent.
#[cfg(feature = "standalone")]
struct Sent(std::sync::mpsc::Sender<Commit>);

#[cfg(feature = "standalone")]
impl Host for Sent {
    fn carry(&mut self, commit: &Commit) -> warren_snail::Result<()> {
        self.0
            .send(commit.clone())
            .map_err(|e| warren_snail::Error::new(e.to_string()))
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_follower_carries_its_leaders_mark_and_refuses_a_stray_writer_while_the_leaders_commits_land() {
    // Written alike, the follower is the leader byte for byte, in every file.
    let s = sample(2_000);
    let leader = snail("replica-leader", &s);
    let follower = snail("replica-follower", &s);
    let files = |p: &Path| {
        let mut all = vec![std::fs::read(p).unwrap()];
        for n in 1.. {
            match std::fs::read(format!("{}.{n}", p.display())) {
                Ok(b) => all.push(b),
                Err(_) => break,
            }
        }
        all
    };
    assert_eq!(files(&follower), files(&leader));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    w.mark_replica(Some(SOURCE)).unwrap();
    let marked = rx.recv().unwrap();
    assert_eq!(marked.replica(), Some(SOURCE.to_string()));
    warren_snail::carry::apply(&follower, &marked).unwrap();
    assert_eq!(Snail::open(&follower).unwrap().replica(), Some(SOURCE));
    w.applying(SOURCE).unwrap();
    w.execute(&station(1, "Sagres 1")).unwrap();
    let next = rx.recv().unwrap();
    // A stray writer on the follower is refused by name, and the leader's commit lands after it.
    let mut stray = Snail::open_writing(&follower).unwrap();
    assert!(refused(stray.execute(&station(2, "Stray")).unwrap_err()));
    drop(stray);
    warren_snail::carry::apply(&follower, &next).unwrap();
    assert_eq!(files(&follower), files(&leader));
    assert_eq!(
        (named(&follower, "Sagres 1"), named(&follower, "Stray")),
        (1, 0)
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_snail_of_format_seven_or_eight_is_marked_no_replica() {
    use warren_snail::catalog::Catalog;
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (a integer NOT NULL, b bigint NOT NULL, PRIMARY KEY (a, b));",
    )
    .unwrap();
    for (n, path) in [
        (7, tmp("replica-seven.snail")),
        (8, tmp("replica-eight.snail")),
    ] {
        match n {
            7 => warren_snail::write::write_format_seven(&path, &catalog, &[], span()),
            _ => warren_snail::write::write_format_eight(&path, &catalog, &[], span()),
        }
        .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        let e = w.mark_replica(Some(SOURCE)).unwrap_err().to_string();
        assert!(
            e.contains(&format!(
                "a snail of format {n} is marked a replica of no source: `out`, then `write`"
            )),
            "{e}"
        );
        // Refused as it is asked, in a transaction too.
        w.execute("BEGIN").unwrap();
        assert!(w.mark_replica(Some(SOURCE)).is_err());
        w.execute("ROLLBACK").unwrap();
        assert_eq!(w.mark_replica(None).unwrap().pages_written, 0);
    }
}

#[test]
#[cfg(feature = "standalone")]
fn the_command_marks_and_clears_a_replica_and_refuses_exec_and_write_on_it_by_name() {
    use std::process::Command;
    let s = sample(2_000);
    let path = snail("replica-command", &s);
    let file = path.to_str().unwrap();
    let bin = env!("CARGO_BIN_EXE_warren-snail");
    let run = |args: &[&str]| Command::new(bin).args(args).output().unwrap();
    let said = |o: &std::process::Output| {
        (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).trim().to_string(),
            String::from_utf8_lossy(&o.stderr).trim().to_string(),
        )
    };
    assert_eq!(said(&run(&["replica", file])).1, "no replica");
    let (ok, out, _) = said(&run(&["replica", file, "--of", SOURCE]));
    assert!(
        ok && out.starts_with(&format!("a replica of {SOURCE}, ")),
        "{out}"
    );
    assert_eq!(
        said(&run(&["replica", file])).1,
        format!("a replica of {SOURCE}")
    );
    let sql = tmp("replica-command.sql");
    std::fs::write(&sql, station(1, "Sagres 1") + ";\n").unwrap();
    let done = tmp("replica-command.parquet");
    let exec = |applier: Option<&str>| {
        let mut args = vec![
            "exec",
            file,
            sql.to_str().unwrap(),
            "--out",
            done.to_str().unwrap(),
        ];
        if let Some(a) = applier {
            args.extend(["--applier", a]);
        }
        said(&run(&args))
    };
    let (ok, _, err) = exec(None);
    assert!(
        !ok && refused(warren_snail::Error::new(err.clone())),
        "{err}"
    );
    let (ok, _, err) = exec(Some("mysql-other"));
    assert!(
        !ok && refused(warren_snail::Error::new(err.clone())),
        "{err}"
    );
    let (ok, _, err) = exec(Some(SOURCE));
    assert!(ok, "{err}");
    assert_eq!(named(&path, "Sagres 1"), 1);
    // `write` makes a new snail where no file is yet, and names the mark of the one that is.
    let ddl = tmp("replica-command-ddl.sql");
    std::fs::write(&ddl, DDL).unwrap();
    let (ok, _, err) = said(&run(&["write", file, "--ddl", ddl.to_str().unwrap()]));
    assert!(
        !ok && err.contains(&format!(
            "the snail there is marked a replica of '{SOURCE}'"
        )),
        "{err}"
    );
    let (ok, out, err) = said(&run(&["replica", file, "--of", "mysql-other"]));
    assert!(!ok && err.contains("clear the mark"), "{out} {err}");
    let (ok, out, _) = said(&run(&["replica", file, "--clear"]));
    assert!(ok && out.starts_with("no replica, "), "{out}");
    std::fs::write(&sql, station(2, "Sagres 2") + ";\n").unwrap();
    let (ok, _, err) = exec(None);
    assert!(ok, "{err}");
    assert_eq!(named(&path, "Sagres 2"), 1);
    let (ok, _, err) = said(&run(&["replica", file, "--of", SOURCE, "--clear"]));
    assert!(!ok && err.contains("not both"), "{err}");
}
