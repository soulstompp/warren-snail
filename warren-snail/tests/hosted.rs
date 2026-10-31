//! A snail written inside a host that decides its commits, as inside PostgreSQL: a transaction's
//! root carries its host's tag and is taken only where the host says that transaction committed, a
//! round with no host to ask reading the root before it, whole; a root whose transaction did not
//! commit is written over by the next commit of its generation; a sequence's advance lands at once;
//! and a host's recovery replays its log of commits on a file that may be past some of them.

mod weather;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use warren_snail::carry::{self, Commit, Host, Judge, Replayed};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::*;

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

/// A judge that takes the tags its host says committed, as it says them.
#[derive(Clone, Default)]
struct Said(Rc<RefCell<Vec<u64>>>);

impl Said {
    fn committed(&self, tag: u64) {
        self.0.borrow_mut().push(tag);
    }
}

impl Judge for Said {
    fn taken(&self, tag: u64) -> warren_snail::Result<bool> {
        Ok(self.0.borrow().contains(&tag))
    }
}

/// A writer inside a host tagging with `tag`, judged by `said`, and the log of what it carries.
fn hosted(path: &Path, tag: u64, said: &Said) -> (Snail, Arc<Mutex<Vec<Commit>>>) {
    let log = Arc::new(Mutex::new(Vec::new()));
    let host = Tagging {
        tag,
        log: log.clone(),
    };
    let w = Snail::open_carried(path, Box::new(host), Rc::new(said.clone())).unwrap();
    (w, log)
}

fn count(s: &mut Snail, q: &str) -> i64 {
    one(&s.ask(q, None).unwrap().0)
}

/// The generation a reader takes, and the stations named `name` it finds there.
fn read_as(path: &Path, judge: Option<&Said>, name: &str) -> (u64, i64) {
    let mut s = match judge {
        Some(j) => Snail::open_judged(path, Rc::new(j.clone())).unwrap(),
        None => Snail::open(path).unwrap(),
    };
    let n = count(
        &mut s,
        &format!("SELECT count(*) FROM weather.stations s WHERE s.name = '{name}'"),
    );
    (s.generation(), n)
}

fn station(no: i64, name: &str) -> String {
    format!(
        "INSERT INTO weather.stations (network_id, station_no, name) VALUES (9001, {no}, '{name}')"
    )
}

/// A copy of a snail's file, made with no snail open on it.
fn copied(from: &Path, to: &Path) -> PathBuf {
    let _ = std::fs::remove_file(to);
    std::fs::copy(from, to).unwrap();
    to.to_path_buf()
}

#[test]
fn a_transactions_root_carries_its_hosts_tag_and_is_taken_only_where_its_host_says_it_committed() {
    let s = sample(2_000);
    let path = snail("hosted-tag", &s);
    let g = Snail::open(&path).unwrap().generation();
    let said = Said::default();
    let (mut w, log) = hosted(&path, 7, &said);
    w.execute(&station(1, "Sagres 1")).unwrap();
    drop(w);
    let carried = log.lock().unwrap().clone();
    assert_eq!(carried.len(), 1);
    assert_eq!((carried[0].generation, carried[0].host()), (g + 1, Some(7)));
    // Before its host's word, and to a round with no host to ask, the snail stands at the parent,
    // whole.
    assert_eq!(read_as(&path, Some(&said), "Sagres 1"), (g, 0));
    assert_eq!(read_as(&path, None, "Sagres 1"), (g, 0));
    said.committed(7);
    assert_eq!(read_as(&path, Some(&said), "Sagres 1"), (g + 1, 1));
    assert_eq!(read_as(&path, None, "Sagres 1"), (g, 0));
    // The next transaction begins from it, and its own root lies beside it: a round with no host
    // to ask takes the first then.
    let (mut w, log) = hosted(&path, 8, &said);
    w.execute(&station(2, "Sagres 2")).unwrap();
    drop(w);
    assert_eq!(log.lock().unwrap()[0].parent, g + 1);
    assert_eq!(read_as(&path, None, "Sagres 1"), (g + 1, 1));
    assert_eq!(read_as(&path, None, "Sagres 2"), (g + 1, 0));
    assert_eq!(read_as(&path, Some(&said), "Sagres 2"), (g + 1, 0));
    said.committed(8);
    assert_eq!(read_as(&path, Some(&said), "Sagres 2"), (g + 2, 1));
}

#[test]
fn a_root_whose_transaction_did_not_commit_is_written_over_by_the_next_commit_of_its_generation() {
    let s = sample(2_000);
    let path = snail("hosted-aborted", &s);
    let g = Snail::open(&path).unwrap().generation();
    let said = Said::default();
    let (mut w, _) = hosted(&path, 7, &said);
    w.execute(&station(1, "Sagres 1")).unwrap();
    drop(w);
    // Transaction 7 never committed: the next writer begins from the parent, and its root takes
    // the copy 7's lay in.
    let (mut w, log) = hosted(&path, 9, &said);
    w.execute(&station(3, "Sagres 3")).unwrap();
    drop(w);
    let c = log.lock().unwrap()[0].clone();
    assert_eq!((c.parent, c.generation, c.host()), (g, g + 1, Some(9)));
    said.committed(9);
    assert_eq!(read_as(&path, Some(&said), "Sagres 3"), (g + 1, 1));
    assert_eq!(read_as(&path, Some(&said), "Sagres 1"), (g + 1, 0));
    // Said committed afterwards, 7's root is gone: written over, never taken.
    said.committed(7);
    assert_eq!(read_as(&path, Some(&said), "Sagres 1"), (g + 1, 0));
}

const SEQUENCED: &str = "CREATE SEQUENCE shop.ids;\n\
     CREATE TABLE shop.orders (id bigint NOT NULL DEFAULT nextval('shop.ids'), v integer NOT NULL, \
     PRIMARY KEY (id));";

#[test]
fn a_sequences_advance_inside_a_host_lands_at_once_untagged() {
    let path = tmp("hosted-sequence.snail");
    let catalog = Catalog::parse(SEQUENCED).unwrap();
    write(&path, &catalog, &[]).unwrap();
    let g = Snail::open(&path).unwrap().generation();
    let said = Said::default();
    let (mut w, log) = hosted(&path, 7, &said);
    w.execute("INSERT INTO shop.orders (v) VALUES (1)").unwrap();
    drop(w);
    let carried = log.lock().unwrap().clone();
    let kinds: Vec<(u64, Option<u64>)> = carried.iter().map(|c| (c.generation, c.host())).collect();
    assert_eq!(kinds, [(g + 1, None), (g + 2, Some(7))]);
    // Transaction 7 never committed: its row is not there, the sequence's advance is.
    let s = Snail::open(&path).unwrap();
    assert_eq!(s.generation(), g + 1);
    let advanced = s.sequences().unwrap();
    assert_eq!(advanced.len(), 1);
    assert!(advanced[0].2, "{advanced:?}");
    let mut s = Snail::open_judged(&path, Rc::new(said.clone())).unwrap();
    assert_eq!(count(&mut s, "SELECT count(*) FROM shop.orders"), 0);
    said.committed(7);
    let mut s = Snail::open_judged(&path, Rc::new(said.clone())).unwrap();
    assert_eq!(
        (
            s.generation(),
            count(&mut s, "SELECT count(*) FROM shop.orders")
        ),
        (g + 2, 1)
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_writer_with_no_host_to_ask_refuses_a_snail_whose_newest_root_waits_on_its_host() {
    let s = sample(2_000);
    let path = snail("hosted-unjudged", &s);
    let said = Said::default();
    let (mut w, _) = hosted(&path, 7, &said);
    w.execute(&station(1, "Sagres 1")).unwrap();
    drop(w);
    let mut plain = Snail::open_writing(&path).unwrap();
    let e = plain.execute(&station(2, "Sagres 2")).unwrap_err();
    assert!(
        e.to_string()
            .contains("waits on its host's word on its transaction 7"),
        "{e}"
    );
}

/// A log of commits made inside a host on a copy of `base`: a station, then each statement on its
/// own, every transaction committed.
fn logged(base: &Path, name: &str, statements: &[String]) -> (PathBuf, Vec<Commit>, Said) {
    let leader = copied(base, &tmp(&format!("{name}-leader.snail")));
    let said = Said::default();
    let mut log = Vec::new();
    for (k, st) in statements.iter().enumerate() {
        let tag = 100 + k as u64;
        let (mut w, carried) = hosted(&leader, tag, &said);
        w.execute(st).unwrap();
        drop(w);
        said.committed(tag);
        log.extend(carried.lock().unwrap().iter().cloned());
    }
    (leader, log, said)
}

fn replayed(path: &Path, c: &Commit) -> Replayed {
    carry::replay(path, c, &mut |_| Ok(())).unwrap()
}

#[test]
fn a_replay_lands_a_commit_on_its_parent_and_leaves_one_it_finds_landed_or_passed() {
    let s = sample(2_000);
    let base = snail("hosted-replay", &s);
    let statements = vec![
        station(1, "Sagres 1"),
        station(2, "Sagres 2"),
        "UPDATE weather.stations SET name = 'Sagres 9' WHERE network_id = 9001 AND station_no = 1"
            .to_string(),
    ];
    let (leader, log, said) = logged(&base, "hosted-replay", &statements);
    let follower = copied(&base, &tmp("hosted-replay-follower.snail"));
    for c in &log {
        assert_eq!(replayed(&follower, c), Replayed::Landed);
        // Read again from the log, it is found landed.
        assert_eq!(replayed(&follower, c), Replayed::Already);
    }
    assert_eq!(
        std::fs::read(&follower).unwrap(),
        std::fs::read(&leader).unwrap()
    );
    // Read again from the log's start, every commit is landed or passed, and nothing moves.
    let before = std::fs::read(&follower).unwrap();
    let again: Vec<Replayed> = log.iter().map(|c| replayed(&follower, c)).collect();
    assert_eq!(
        again,
        [Replayed::Passed, Replayed::Already, Replayed::Already]
    );
    assert_eq!(std::fs::read(&follower).unwrap(), before);
    assert_eq!(read_as(&follower, Some(&said), "Sagres 9").1, 1);
    // A commit that begins past where the follower stands is refused by name.
    let behind = copied(&base, &tmp("hosted-replay-behind.snail"));
    let e = carry::replay(&behind, &log[1], &mut |_| Ok(())).unwrap_err();
    assert!(
        e.to_string().contains("and the snail stands at generation"),
        "{e}"
    );
}

#[test]
fn a_replay_lands_a_later_root_of_one_generation_over_an_earlier_one_its_host_never_took() {
    let s = sample(2_000);
    let base = snail("hosted-siblings", &s);
    let leader = copied(&base, &tmp("hosted-siblings-leader.snail"));
    let said = Said::default();
    let (mut w, first) = hosted(&leader, 7, &said);
    w.execute(&station(1, "Sagres 1")).unwrap();
    drop(w);
    let (mut w, second) = hosted(&leader, 9, &said);
    w.execute(&station(3, "Sagres 3")).unwrap();
    drop(w);
    said.committed(9);
    let (a, b) = (
        first.lock().unwrap()[0].clone(),
        second.lock().unwrap()[0].clone(),
    );
    assert_eq!(
        (a.generation, a.host(), b.generation, b.host()),
        (b.generation, Some(7), a.generation, Some(9))
    );
    let follower = copied(&base, &tmp("hosted-siblings-follower.snail"));
    assert_eq!(replayed(&follower, &a), Replayed::Landed);
    assert_eq!(replayed(&follower, &b), Replayed::Landed);
    assert_eq!(
        std::fs::read(&follower).unwrap(),
        std::fs::read(&leader).unwrap()
    );
    // A follower that took the later one already, its log read again from the earlier: the
    // earlier lands over it, and the later over that, as the log ran.
    assert_eq!(replayed(&follower, &a), Replayed::Landed);
    assert_eq!(read_as(&follower, Some(&said), "Sagres 3").1, 0);
    assert_eq!(replayed(&follower, &b), Replayed::Landed);
    assert_eq!(
        std::fs::read(&follower).unwrap(),
        std::fs::read(&leader).unwrap()
    );
    assert_eq!(
        read_as(&follower, Some(&said), "Sagres 3"),
        (b.generation, 1)
    );
}

#[test]
fn a_replay_waits_for_a_round_holding_a_generation_its_commit_frees_and_an_error_from_the_wait_ends_it(
) {
    let s = sample(4_000);
    let base = snail("hosted-wait", &s);
    let away = |k: usize| s.reading_id[s.reading_id.len() * k / 7];
    let statements: Vec<String> = (1..6)
        .map(|k| {
            format!(
                "DELETE FROM weather.readings WHERE reading_id = {}",
                away(k)
            )
        })
        .collect();
    let (_, log, said) = logged(&base, "hosted-wait", &statements);
    let follower = copied(&base, &tmp("hosted-wait-follower.snail"));
    let g = Snail::open(&follower).unwrap().generation();
    // A round on the follower holds the generation it began at, while the log is replayed.
    let holding = Snail::open(&follower).unwrap();
    let mut stopped = None;
    for c in &log {
        let mut asked = Vec::new();
        let r = carry::replay(&follower, c, &mut |tag| {
            asked.push(tag);
            Err(warren_snail::Error::new("the round was cancelled"))
        });
        if let Some(&tag) = asked.first() {
            assert!(tag > g, "{tag} {g}");
            assert!(r
                .unwrap_err()
                .to_string()
                .contains("the round was cancelled"));
            stopped = Some(c.clone());
            break;
        }
        assert_eq!(r.unwrap(), Replayed::Landed);
    }
    let stopped = stopped.expect("a commit of the log frees pages the held generation names");
    assert_eq!(
        Snail::open_judged(&follower, Rc::new(said.clone()))
            .unwrap()
            .generation(),
        stopped.parent,
        "an ended replay writes nothing"
    );
    drop(holding);
    let mut calls = 0;
    assert_eq!(
        carry::replay(&follower, &stopped, &mut |_| {
            calls += 1;
            Ok(())
        })
        .unwrap(),
        Replayed::Landed
    );
    assert_eq!(calls, 0);
}
