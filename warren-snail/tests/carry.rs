//! Commits carried to followers: a follower given each commit is the leader byte for byte and answers
//! as the leader at every generation; one stopped before a commit's root, or halfway through it,
//! stands at the commit's parent; a follower's round keeps the pages its generation names, the apply
//! waiting for it to end; a new follower is a byte copy of the leader; and with no host, a commit
//! writes its root as it always has.

mod weather;

#[cfg(feature = "standalone")]
use std::cell::RefCell;
#[cfg(feature = "standalone")]
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
#[cfg(feature = "standalone")]
use std::rc::Rc;
#[cfg(feature = "standalone")]
use warren_snail::carry::{self, Commit, Host};
#[cfg(feature = "standalone")]
use warren_snail::file::Stop;
#[cfg(feature = "standalone")]
use warren_snail::read::Snail;
use weather::*;

/// Every file of a snail: the first, then `<path>.1` and on.
fn files_of(path: &Path) -> Vec<PathBuf> {
    let mut out = vec![path.to_path_buf()];
    for n in 1.. {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        let p = PathBuf::from(p);
        if !p.exists() {
            break;
        }
        out.push(p);
    }
    out
}

/// Whether two snails' files are the same, file for file and byte for byte.
fn same_bytes(a: &Path, b: &Path) -> bool {
    let (x, y) = (files_of(a), files_of(b));
    x.len() == y.len()
        && x.iter()
            .zip(&y)
            .all(|(p, q)| std::fs::read(p).unwrap() == std::fs::read(q).unwrap())
}

/// Nothing left at a snail's path, nor at any of its further files.
fn removed(path: &Path) -> PathBuf {
    for p in files_of(path) {
        let _ = std::fs::remove_file(p);
    }
    path.to_path_buf()
}

/// A copy of a snail's files, made with no snail open on them.
fn copied(from: &Path, to: &Path) -> PathBuf {
    removed(to);
    for (n, p) in files_of(from).iter().enumerate() {
        let mut q = to.as_os_str().to_owned();
        if n > 0 {
            q.push(format!(".{n}"));
        }
        std::fs::copy(p, PathBuf::from(q)).unwrap();
    }
    to.to_path_buf()
}

#[cfg(feature = "standalone")]
fn generation(path: &Path) -> u64 {
    Snail::open(path).unwrap().generation()
}

/// What a commit begins from: its generation, the pages its root names the file as holding, and the
/// free map's runs.
#[cfg(feature = "standalone")]
struct Parent {
    generation: u64,
    pages: u64,
    free: Vec<warren_snail::file::Run>,
}

#[cfg(feature = "standalone")]
fn parent(path: &Path) -> Parent {
    let r = warren_snail::file::Reader::open(path).unwrap();
    let root = r.root().unwrap();
    let free = if root.free.bytes > 0 {
        warren_snail::file::decode_runs(&r.read_extent(root.free).unwrap()).unwrap()
    } else {
        Vec::new()
    };
    Parent {
        generation: root.generation,
        pages: root.pages,
        free,
    }
}

/// A commit's page by its number in the whole snail.
#[cfg(feature = "standalone")]
fn number(p: &carry::Page, span: u64) -> u64 {
    p.file * span + p.page
}

/// A host whose majority is its followers: each commit applied to every one of them before it
/// returns, and kept.
#[cfg(feature = "standalone")]
struct Followers {
    paths: Vec<PathBuf>,
    log: Rc<RefCell<Vec<Commit>>>,
}

#[cfg(feature = "standalone")]
impl Host for Followers {
    fn carry(&mut self, commit: &Commit) -> warren_snail::Result<()> {
        for p in &self.paths {
            carry::apply(p, commit)?;
        }
        self.log.borrow_mut().push(commit.clone());
        Ok(())
    }
}

#[cfg(feature = "standalone")]
fn followed(paths: &[&Path]) -> (Box<dyn Host>, Rc<RefCell<Vec<Commit>>>) {
    let log = Rc::new(RefCell::new(Vec::new()));
    let host = Followers {
        paths: paths.iter().map(|p| p.to_path_buf()).collect(),
        log: log.clone(),
    };
    (Box::new(host), log)
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

/// The replication spike's stream on the weather sample, a commit an item: a station added and a
/// reading written through it; a reading of the busiest station; a reading checked; a feed
/// corrected; a station renamed, which moves it on the names' index; a reading moved to another
/// station, which moves it on the table's order; a reading deleted; a transaction of five; a reading
/// written, checked and deleted again, once pages are being reused; and every reading of the
/// busiest station deleted.
fn stream(s: &Sample) -> Vec<Vec<String>> {
    let (bn, bs) = busiest(s);
    let (n1, s1, _) = in_order(s)[10];
    let (n2, s2, _) = in_order(s)[s.reading_id.len() / 2];
    let away = |k: usize| s.reading_id[s.reading_id.len() * k / 7];
    let insert = |id: i64, n: i64, st: i64, at: &str, feed: &str| {
        format!(
            "INSERT INTO weather.readings (reading_id, network_id, station_no, feed, observed_at, checked_at) \
             SELECT {id}, s.network_id, s.station_no, '{feed}', '{at}', NULL \
             FROM weather.stations s WHERE s.network_id = {n} AND s.station_no = {st}"
        )
    };
    let checked = |id: i64, at: &str| {
        format!("UPDATE weather.readings SET checked_at = '{at}' WHERE reading_id = {id}")
    };
    let deleted = |id: i64| format!("DELETE FROM weather.readings WHERE reading_id = {id}");
    let next = 2_000_000;
    vec![
        vec!["INSERT INTO weather.stations (network_id, station_no, name) VALUES (9001, 1, 'Sagres 1')".into()],
        vec![insert(next, 9001, 1, "2022-12-05 10:00:00+00", "AWS Main")],
        vec![insert(next + 1, bn, bs, "2022-12-06 11:00:00+00", "observer")],
        vec![checked(next, "2022-12-08 10:00:00+00")],
        vec![format!("UPDATE weather.readings SET feed = 'AWS Backup' WHERE reading_id = {}", away(1))],
        vec![format!("UPDATE weather.stations SET name = 'Porto Santo 99' WHERE network_id = {n1} AND station_no = {s1}")],
        vec![format!("UPDATE weather.readings SET network_id = 9001, station_no = 1 WHERE reading_id = {}", away(2))],
        vec![deleted(away(3))],
        vec![
            "BEGIN".into(),
            "INSERT INTO weather.stations (network_id, station_no, name) VALUES (9001, 2, 'Sagres 2')".into(),
            insert(next + 2, 9001, 2, "2021-12-24 09:00:00+00", "synop relay"),
            checked(next + 2, "2021-12-28 09:00:00+00"),
            format!("UPDATE weather.stations SET name = 'Faro 0' WHERE network_id = {n2} AND station_no = {s2}"),
            deleted(away(4)),
            "COMMIT".into(),
        ],
        vec![insert(next + 3, bn, bs, "2023-12-01 12:00:00+00", "AWS hourly")],
        vec![checked(next + 3, "2023-12-04 12:00:00+00")],
        vec![deleted(away(5))],
        vec![format!("DELETE FROM weather.readings WHERE network_id = {bn} AND station_no = {bs}")],
    ]
}

/// The questions a follower is held to the leader on: every kind of read, along every order that
/// serves it, the rows the stream changes among them.
fn questions(s: &Sample) -> Vec<String> {
    let (bn, bs) = busiest(s);
    let join =
        "JOIN weather.stations s ON s.network_id = r.network_id AND s.station_no = r.station_no";
    vec![
        "SELECT count(*) FROM weather.readings r".into(),
        "SELECT r.feed, count(*) FROM weather.readings r GROUP BY r.feed".into(),
        format!("SELECT {MONTH} AS m, count(*) FROM weather.readings r WHERE {SPAN} GROUP BY 1"),
        format!("SELECT r.checked_at FROM weather.readings r WHERE {MONTH} = 12 AND {SPAN}"),
        "SELECT r.reading_id, r.station_no, r.checked_at FROM weather.readings r WHERE r.network_id = 9001".into(),
        format!("SELECT r.reading_id, r.feed FROM weather.readings r WHERE r.network_id = {bn} AND r.station_no = {bs}"),
        "SELECT s.network_id, s.station_no FROM weather.stations s WHERE s.name = 'Porto Santo 99'".into(),
        "SELECT s.name, count(*) FROM weather.stations s GROUP BY s.name".into(),
        "SELECT r.reading_id FROM weather.readings r WHERE r.reading_id >= 2000000".into(),
        format!("SELECT s.name, count(*) FROM weather.readings r {join} WHERE {SPAN} GROUP BY s.name"),
    ]
}

/// Each commit of the stream executed on a writer, statement by statement: the last statement's
/// account of each.
#[cfg(feature = "standalone")]
fn run(w: &mut Snail, commit: &[String]) -> warren_snail::read::Done {
    let mut done = None;
    for st in commit {
        done = Some(w.execute(st).unwrap());
    }
    done.unwrap()
}

#[test]
#[cfg(feature = "standalone")]
fn a_follower_given_each_commit_is_the_leader_byte_for_byte_and_answers_as_it() {
    let s = sample(20_000);
    let base = snail("carry-base", &s);
    let leader = copied(&base, &tmp("carry-leader.snail"));
    let follower = copied(&base, &tmp("carry-follower.snail"));
    let plain = copied(&base, &tmp("carry-plain.snail"));
    let span = span();
    let mut w = Snail::open_writing(&leader).unwrap();
    let (host, log) = followed(&[&follower]);
    w.carried_by(host).unwrap();
    let mut without = Snail::open_writing(&plain).unwrap();
    let mut reused = 0;
    for (k, commit) in stream(&s).iter().enumerate() {
        let before = parent(&follower);
        let d = run(&mut w, commit);
        let c = log.borrow().last().unwrap().clone();
        assert_eq!(log.borrow().len(), k + 1);
        assert_eq!(
            (c.parent, c.generation, d.generation),
            (
                before.generation,
                before.generation + 1,
                before.generation + 1
            ),
            "commit {k}"
        );
        // The pages it wrote, the root's but one, and none its parent names: past the parent's last
        // page, or in its free map.
        assert_eq!(c.pages.len() as u64 + 1, d.pages_written, "commit {k}");
        for p in &c.pages {
            let n = number(p, span);
            assert!(
                n >= 2
                    && (n >= before.pages
                        || before
                            .free
                            .iter()
                            .any(|r| n >= r.start && n < r.start + r.pages)),
                "commit {k} wrote page {n}, which generation {} names",
                before.generation
            );
        }
        // Those it wrote into freed pages are the pages it reused.
        let into_free = c
            .pages
            .iter()
            .filter(|p| number(p, span) < before.pages)
            .count() as u64;
        assert_eq!(into_free, d.pages_reused, "commit {k}");
        reused += d.pages_reused;
        assert!(same_bytes(&leader, &follower), "commit {k}");
        // With no host, the same statements write the same pages and the same root.
        let e = run(&mut without, commit);
        assert_eq!(
            (e.generation, e.pages_written, e.pages_reused),
            (d.generation, d.pages_written, d.pages_reused)
        );
        assert!(same_bytes(&leader, &plain), "commit {k}");
    }
    assert!(reused > 0, "the stream reused no page");
    drop(w);
    drop(without);
    Snail::open(&follower).unwrap().census().unwrap();
    same_answers(
        &mut Snail::open(&follower).unwrap(),
        &mut Snail::open(&leader).unwrap(),
        &questions(&s),
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_follower_stopped_before_a_commits_root_or_halfway_through_it_stands_at_its_parent() {
    let s = sample(20_000);
    let base = snail("stopped-base", &s);
    let leader = copied(&base, &tmp("stopped-leader.snail"));
    let follower = copied(&base, &tmp("stopped-follower.snail"));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    let mut stopped_at = Vec::new();
    for (k, commit) in stream(&s).iter().enumerate() {
        let d = run(&mut w, commit);
        let c = rx.recv().unwrap();
        // At the first commit, and at the first that writes into freed pages.
        if k == 0 || (d.pages_reused > 0 && stopped_at.len() < 2) {
            let at = copied(&follower, &tmp(&format!("stopped-{k}-parent.snail")));
            for stop in [Stop::BeforeRoot, Stop::HalfRoot] {
                let f = copied(&follower, &tmp(&format!("stopped-{k}-{stop:?}.snail")));
                let e = carry::apply_stopping(&f, &c, Some(stop))
                    .unwrap_err()
                    .to_string();
                assert!(e.contains("stopped"), "{e}");
                assert_eq!(generation(&f), c.parent, "commit {k}, {stop:?}");
                Snail::open(&f).unwrap().census().unwrap();
                same_answers(
                    &mut Snail::open(&f).unwrap(),
                    &mut Snail::open(&at).unwrap(),
                    &questions(&s),
                );
                // Applied again, it is finished.
                carry::apply(&f, &c).unwrap();
                assert!(same_bytes(&f, &leader), "commit {k}, {stop:?}");
            }
            stopped_at.push(k);
        }
        carry::apply(&follower, &c).unwrap();
        assert!(same_bytes(&follower, &leader), "commit {k}");
    }
    assert_eq!(stopped_at.len(), 2, "no commit reused a page");
}

#[test]
#[cfg(feature = "standalone")]
fn a_round_on_a_follower_keeps_the_pages_its_generation_names_and_the_apply_waits_for_it() {
    let s = sample(20_000);
    let base = snail("waits-base", &s);
    let leader = copied(&base, &tmp("waits-leader.snail"));
    let follower = copied(&base, &tmp("waits-follower.snail"));
    let qs = questions(&s);
    let at_one = |q: &str| Snail::open(&base).unwrap().ask(q, None).unwrap().0;
    // A round on the follower, at the first generation.
    let mut round = Snail::open(&follower).unwrap();
    assert_eq!(round.generation(), 1);
    let (tx, rx) = std::sync::mpsc::channel::<Commit>();
    let (applied_tx, applied) = std::sync::mpsc::channel();
    let f = follower.clone();
    let applier = std::thread::spawn(move || {
        for c in rx {
            carry::apply(&f, &c).unwrap();
            applied_tx.send(c.generation).unwrap();
        }
    });
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    let mut last = 0;
    for commit in stream(&s) {
        last = run(&mut w, &commit).generation;
    }
    // The first commit frees nothing; the second frees the pages the first stopped naming, which
    // the round's generation names: its apply waits.
    let wait = std::time::Duration::from_secs(10);
    assert_eq!(applied.recv_timeout(wait).unwrap(), 2);
    assert!(
        applied
            .recv_timeout(std::time::Duration::from_millis(500))
            .is_err(),
        "the follower applied a commit that frees pages its round's generation names"
    );
    // The round reads the generation it began at, every page as it was.
    let (got, _) = round.ask(&qs[5], None).unwrap();
    assert_eq!(bag(&got), bag(&at_one(&qs[5])));
    drop(round);
    while applied.recv_timeout(wait).unwrap() < last {}
    drop(w);
    applier.join().unwrap();
    assert!(same_bytes(&follower, &leader));

    // A round at the follower's newest generation holds no commit after it: the next one frees only
    // pages older generations name.
    let mut newest = Snail::open(&follower).unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<Commit>();
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    w.execute("UPDATE weather.readings SET feed = 'observer' WHERE reading_id = 2000000")
        .unwrap();
    let c = rx.recv().unwrap();
    let f = follower.clone();
    let one = std::thread::spawn(move || carry::apply(&f, &c));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        one.is_finished(),
        "a round at the newest generation held the next commit"
    );
    one.join().unwrap().unwrap();
    assert_eq!(newest.generation(), last);
    newest.ask(&qs[0], None).unwrap();
}

#[test]
#[cfg(feature = "standalone")]
fn pages_a_leader_reuses_are_read_by_an_older_round_unless_it_holds_their_generation() {
    let s = sample(20_000);
    let base = snail("reused-base", &s);
    let qs = questions(&s);
    let answers = |path: &Path| -> Vec<Result<Vec<Vec<Option<String>>>, String>> {
        let mut r = Snail::open(path).unwrap();
        qs.iter()
            .map(|q| {
                r.ask(q, None)
                    .map(|(b, _)| bag(&b))
                    .map_err(|e| e.to_string())
            })
            .collect()
    };
    let first = answers(&base);
    for held in [false, true] {
        let leader = copied(&base, &tmp(&format!("reused-{held}-leader.snail")));
        let follower = copied(&base, &tmp(&format!("reused-{held}-follower.snail")));
        // The pages free when the snail was written: none in one file, the pages passed over at each
        // file's end in several.
        let free = Snail::open(&leader).unwrap().census().unwrap().free;
        let hold = held.then(|| Snail::open(&leader).unwrap());
        let mut w = Snail::open_writing(&leader).unwrap();
        let (host, _) = followed(&[&follower]);
        w.carried_by(host).unwrap();
        let mut reused = 0;
        for commit in stream(&s) {
            reused += run(&mut w, &commit).pages_reused;
        }
        drop(w);
        drop(hold);
        // The stream's pages under the first generation's root: a round that began at the first
        // generation, reading the follower after the stream.
        let stale = copied(&follower, &tmp(&format!("reused-{held}-stale.snail")));
        let mut both = vec![0u8; 2 * 4096];
        std::fs::File::open(&base)
            .unwrap()
            .read_exact_at(&mut both, 0)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&stale)
            .unwrap()
            .write_all_at(&both, 0)
            .unwrap();
        if held {
            assert!(reused <= free, "{reused} pages reused, {free} free before");
            assert_eq!(answers(&stale), first);
        } else {
            assert!(reused > free, "{reused} pages reused, {free} free before");
            assert_ne!(answers(&stale), first);
        }
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_follower_takes_only_the_commit_after_its_generation_and_a_commit_landed_again_changes_nothing()
{
    let s = sample(5_000);
    let base = snail("parented-base", &s);
    let leader = copied(&base, &tmp("parented-leader.snail"));
    let follower = copied(&base, &tmp("parented-follower.snail"));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    let commits: Vec<Commit> = stream(&s)[..3]
        .iter()
        .map(|c| {
            run(&mut w, c);
            rx.recv().unwrap()
        })
        .collect();
    let e = carry::apply(&follower, &commits[1])
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("begins from generation 2, and the follower stands at generation 1"),
        "{e}"
    );
    carry::apply(&follower, &commits[0]).unwrap();
    let once = std::fs::read(&follower).unwrap();
    carry::apply(&follower, &commits[0]).unwrap();
    assert_eq!(std::fs::read(&follower).unwrap(), once);
    // A commit naming a root's page, and one whose root names another generation, are refused.
    let mut onto_root = commits[1].clone();
    onto_root.pages[0].page = 1;
    onto_root.pages[0].file = 0;
    let e = carry::apply(&follower, &onto_root).unwrap_err().to_string();
    assert!(e.contains("no page a commit writes"), "{e}");
    let mut later = commits[1].clone();
    later.generation += 1;
    let e = carry::apply(&follower, &later).unwrap_err().to_string();
    assert!(e.contains("holds no whole root of its generation"), "{e}");
    assert_eq!(std::fs::read(&follower).unwrap(), once);
    // Carried as bytes, each commit comes back the same, and a byte changed refuses it.
    for c in &commits[1..] {
        let bytes = c.encode();
        let back = Commit::decode(&bytes).unwrap();
        assert_eq!(&back, c);
        let mut torn = bytes.clone();
        torn[bytes.len() / 2] ^= 1;
        let e = Commit::decode(&torn).unwrap_err().to_string();
        assert!(e.contains("not a snail's commit"), "{e}");
        carry::apply(&follower, &back).unwrap();
    }
    assert!(same_bytes(&follower, &leader));
}

#[test]
fn bytes_that_are_not_a_commit_whole_are_refused() {
    for bytes in [&b""[..], &b"wscommit"[..], &[0u8; 64][..]] {
        let e = warren_snail::carry::Commit::decode(bytes)
            .unwrap_err()
            .to_string();
        assert!(e.contains("not a snail's commit"), "{e}");
    }
}

#[test]
#[cfg(feature = "standalone")]
fn a_new_follower_is_a_byte_copy_of_the_leader_under_a_generation_it_holds() {
    let s = sample(20_000);
    let base = snail("copied-base", &s);
    let leader = copied(&base, &tmp("copied-leader.snail"));
    let follower = removed(&tmp("copied-follower.snail"));
    let commits = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Commit>::new()));
    let (l, kept) = (leader.clone(), commits.clone());
    let (tx, rx) = std::sync::mpsc::channel::<Commit>();
    let keeper = std::thread::spawn(move || {
        for c in rx {
            kept.lock().unwrap().push(c);
        }
    });
    let writes = stream(&s);
    let (started_tx, started) = std::sync::mpsc::channel();
    let (copied_tx, copied_rx) = std::sync::mpsc::channel::<()>();
    // A writer commits while the copy is made, and twice after it, so that each copy of the root is
    // written past the copy's generation.
    let writer = std::thread::spawn(move || {
        let mut w = Snail::open_writing(&l).unwrap();
        w.carried_by(Box::new(Sent(tx))).unwrap();
        for (k, c) in writes.iter().enumerate() {
            run(&mut w, c);
            if k == 2 {
                started_tx.send(()).unwrap();
            }
        }
        copied_rx.recv().unwrap();
        for feed in ["observer", "AWS Main"] {
            w.execute(&format!(
                "UPDATE weather.readings SET feed = '{feed}' WHERE reading_id = 2000000"
            ))
            .unwrap();
        }
    });
    started.recv().unwrap();
    let at = carry::copy(&leader, &follower).unwrap();
    copied_tx.send(()).unwrap();
    writer.join().unwrap();
    keeper.join().unwrap();
    assert!(at >= 4, "{at}");
    assert_eq!(generation(&follower), at);
    Snail::open(&follower).unwrap().census().unwrap();
    for c in commits.lock().unwrap().iter().filter(|c| c.generation > at) {
        carry::apply(&follower, c).unwrap();
    }
    assert!(same_bytes(&follower, &leader));
    let e = carry::copy(&leader, &follower).unwrap_err().to_string();
    assert!(e.contains("where no file lies"), "{e}");
}

#[test]
#[cfg(feature = "standalone")]
fn a_commit_across_a_huge_snails_files_names_each_page_by_its_file_and_lands_in_each() {
    let s = sample(20_000);
    let span = 256;
    let base = snail_spanned("huge-base", &s, span);
    let leader = copied(&base, &tmp("huge-leader.snail"));
    let follower = removed(&tmp("huge-follower.snail"));
    assert_eq!(carry::copy(&leader, &follower).unwrap(), 1);
    assert!(files_of(&follower).len() > 1);
    assert!(same_bytes(&follower, &leader));
    let mut w = Snail::open_writing(&leader).unwrap();
    let (host, log) = followed(&[&follower]);
    w.carried_by(host).unwrap();
    let mut most = 0;
    for commit in stream(&s) {
        let d = run(&mut w, &commit);
        let c = log.borrow().last().unwrap().clone();
        assert_eq!(c.pages.len() as u64 + 1, d.pages_written);
        assert!(c.pages.iter().all(|p| p.page < span));
        let files: std::collections::BTreeSet<u64> = c.pages.iter().map(|p| p.file).collect();
        most = most.max(files.len());
        assert!(same_bytes(&follower, &leader));
    }
    assert!(most > 1, "no commit lay in more than one file");
}

#[test]
#[cfg(feature = "standalone")]
fn a_leader_stopped_once_a_majority_holds_its_commit_takes_it_as_its_followers_did() {
    let s = sample(5_000);
    let base = snail("majority-base", &s);
    let leader = copied(&base, &tmp("majority-leader.snail"));
    let follower = copied(&base, &tmp("majority-follower.snail"));
    let st = stream(&s);
    let mut w = Snail::open_writing(&leader).unwrap();
    let (host, log) = followed(&[&follower]);
    w.carried_by(host).unwrap();
    w.stop_at(Some(Stop::BeforeRoot));
    let e = w.execute(&st[0][0]).unwrap_err().to_string();
    assert!(e.contains("stopped before the root"), "{e}");
    drop(w);
    assert_eq!((generation(&leader), generation(&follower)), (1, 2));
    let c = log.borrow()[0].clone();
    carry::apply(&leader, &c).unwrap();
    assert!(same_bytes(&leader, &follower));

    // A host that does not take the commit leaves the leader at its parent, the commit's pages
    // named by nothing; the next commit it takes is the next generation.
    struct Refusing(bool);
    impl Host for Refusing {
        fn carry(&mut self, _: &Commit) -> warren_snail::Result<()> {
            if std::mem::replace(&mut self.0, false) {
                return Err(warren_snail::Error::new("no majority holds the commit"));
            }
            Ok(())
        }
    }
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Refusing(true))).unwrap();
    let e = w.execute(&st[1][0]).unwrap_err().to_string();
    assert!(e.contains("no majority holds the commit"), "{e}");
    assert_eq!(generation(&leader), 2);
    Snail::open(&leader).unwrap().census().unwrap();
    assert_eq!(w.execute(&st[1][0]).unwrap().generation, 3);
    drop(w);
    let (got, _) = Snail::open(&leader)
        .unwrap()
        .ask(
            "SELECT count(*) FROM weather.readings r WHERE r.reading_id = 2000000",
            None,
        )
        .unwrap();
    assert_eq!(one(&got), 1);
}

/// A host that holds each commit at the root's turn until told to go on, saying first that it holds
/// it.
#[cfg(feature = "standalone")]
struct Held(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>);

#[cfg(feature = "standalone")]
impl Host for Held {
    fn carry(&mut self, _: &Commit) -> warren_snail::Result<()> {
        self.0.send(()).unwrap();
        self.1.recv().unwrap();
        Ok(())
    }
}

/// A writer on its own thread whose one statement's commit its host holds at the root's turn:
/// returned once the host holds it, with the sender that lets it go on, the writer's thread giving
/// back the generation the statement made.
#[cfg(feature = "standalone")]
fn holding(
    path: &Path,
    sql: String,
) -> (std::thread::JoinHandle<u64>, std::sync::mpsc::Sender<()>) {
    let (carrying_tx, carrying) = std::sync::mpsc::channel();
    let (go, go_rx) = std::sync::mpsc::channel();
    let p = path.to_path_buf();
    let writer = std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        w.carried_by(Box::new(Held(carrying_tx, go_rx))).unwrap();
        w.execute(&sql).unwrap().generation
    });
    carrying.recv().unwrap();
    (writer, go)
}

#[test]
#[cfg(feature = "standalone")]
fn commits_take_turns_at_the_root_while_one_waits_for_its_host() {
    let s = sample(20_000);
    let path = snail("turns-host", &s);
    let order = in_order(&s);
    let (a, b) = (order[10].2, order[order.len() - 10].2);
    let (first, go) = holding(
        &path,
        format!("UPDATE weather.readings SET feed = 'one' WHERE reading_id = {a}"),
    );
    // The first commit holds the root's turn while its host carries it: a writer of another block
    // commits only once it is through.
    let p = path.clone();
    let second = std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        w.execute(&format!(
            "UPDATE weather.readings SET feed = 'two' WHERE reading_id = {b}"
        ))
        .unwrap()
        .generation
    });
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !second.is_finished(),
        "a commit took the root's turn meanwhile"
    );
    go.send(()).unwrap();
    assert_eq!((first.join().unwrap(), second.join().unwrap()), (2, 3));
    let mut r = Snail::open(&path).unwrap();
    assert_eq!(
        (feed_of(&mut r, a), feed_of(&mut r, b)),
        ("one".into(), "two".into())
    );
}

#[test]
#[cfg(feature = "standalone")]
fn an_apply_takes_the_followers_root_turn_so_it_never_lands_beside_a_commit_there() {
    let s = sample(5_000);
    let base = snail("beside-base", &s);
    let leader = copied(&base, &tmp("beside-leader.snail"));
    let follower = copied(&base, &tmp("beside-follower.snail"));
    let (tx, rx) = std::sync::mpsc::channel();
    let mut w = Snail::open_writing(&leader).unwrap();
    w.carried_by(Box::new(Sent(tx))).unwrap();
    run(&mut w, &stream(&s)[0]);
    let c = rx.recv().unwrap();
    // A commit on the follower itself, held at the root's turn: the apply waits for it, then finds
    // the follower moved on, and is refused by name.
    let (local, go) = holding(
        &follower,
        format!(
            "DELETE FROM weather.readings WHERE reading_id = {}",
            s.reading_id[0]
        ),
    );
    let f = follower.clone();
    let apply = std::thread::spawn(move || carry::apply(&f, &c));
    std::thread::sleep(std::time::Duration::from_millis(500));
    assert!(
        !apply.is_finished(),
        "the apply wrote beside a commit on the follower"
    );
    go.send(()).unwrap();
    assert_eq!(local.join().unwrap(), 2);
    let e = apply.join().unwrap().unwrap_err().to_string();
    assert!(e.contains("the follower stands at generation 2"), "{e}");
    Snail::open(&follower).unwrap().census().unwrap();
}
