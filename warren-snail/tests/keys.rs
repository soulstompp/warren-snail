//! Foreign keys: each read from the DDL as PostgreSQL reads it, and checked where the round ends, the
//! statement's or, deferred, the commit's: a row written has its parent unless its reference holds
//! a NULL, and a key taken from a parent is acted on, or refused while a row references it.

#![cfg(feature = "standalone")]

mod weather;

use arrow::array::{Array, RecordBatch};
use std::sync::mpsc;
use std::time::Duration;
use warren_snail::catalog::{Action, Catalog};
use warren_snail::read::Snail;
use weather::{tmp, write};

fn made(name: &str, ddl: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    path
}

fn run(w: &mut Snail, sql: &str) {
    w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
}

fn refused(w: &mut Snail, sql: &str, why: &str) {
    let e = w
        .execute(sql)
        .err()
        .unwrap_or_else(|| panic!("{sql} was taken"))
        .to_string();
    assert!(e.contains(why), "{sql}: {e}");
}

/// An answer's rows, each cell as arrow writes it, NULL as `NULL`, sorted.
fn rows(s: &mut Snail, sql: &str) -> Vec<String> {
    let (b, _): (RecordBatch, _) = s.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
    let mut out: Vec<String> = (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|a| {
                    if a.is_null(r) {
                        "NULL".to_string()
                    } else {
                        arrow::util::display::array_value_to_string(a, r).unwrap()
                    }
                })
                .collect::<Vec<_>>()
                .join(",")
        })
        .collect();
    out.sort();
    out
}

fn refused_ddl(ddl: &str) -> String {
    Catalog::parse(ddl).unwrap_err().to_string()
}

const PARENT: &str = "CREATE TABLE s.p (id integer PRIMARY KEY, a integer, b integer, UNIQUE (a, b), d integer UNIQUE DEFERRABLE);";

#[test]
fn a_foreign_key_is_read_as_postgresql_reads_it() {
    let c = Catalog::parse(&format!(
        "{PARENT}
         CREATE UNIQUE INDEX pd ON s.p (d);
         CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.p, y bigint, z integer,
             FOREIGN KEY (z, y) REFERENCES s.p (b, a) MATCH FULL ON DELETE SET NULL (y) ON UPDATE CASCADE,
             w integer CONSTRAINT cw REFERENCES s.p (id) DEFERRABLE INITIALLY DEFERRED NOT ENFORCED);
         CREATE TABLE s.e (id integer PRIMARY KEY, boss integer REFERENCES s.e ON DELETE CASCADE);
         CREATE TABLE s.f (id integer PRIMARY KEY, x integer, x2 integer);
         ALTER TABLE ONLY s.f ADD CONSTRAINT f_x FOREIGN KEY (x) REFERENCES s.p(id) NOT VALID;
         ALTER TABLE s.f ADD FOREIGN KEY (x2) REFERENCES s.p (id) ON DELETE RESTRICT;"
    ))
    .unwrap();
    let c_ = c.table("s.c").unwrap();
    let names: Vec<&str> = c_.foreign_keys.iter().map(|k| k.name.as_str()).collect();
    assert_eq!(names, ["c_x_fkey", "c_z_y_fkey", "cw"]);
    let k = &c_.foreign_keys[1];
    assert_eq!(
        (k.columns.clone(), k.parent_columns.clone()),
        (vec![3, 2], vec![2, 1])
    );
    assert_eq!(k.key, "p_a_b_key");
    assert!(k.full && k.enforced && k.valid && !k.deferrable);
    assert_eq!(
        (k.on_delete.clone(), k.on_update.clone()),
        (Action::SetNull(vec![2]), Action::Cascade)
    );
    let w = &c_.foreign_keys[2];
    assert!(w.deferrable && w.deferred && !w.enforced && !w.valid);
    assert_eq!(c.table("s.e").unwrap().foreign_keys[0].key, "e_pkey");
    let f = c.table("s.f").unwrap();
    assert!(!f.foreign_keys[0].valid && f.foreign_keys[1].valid);
    assert_eq!(f.foreign_keys[1].name, "f_x2_fkey");
    assert_eq!(f.foreign_keys[1].on_delete, Action::Restrict);
    // A table whose reference leads no order is read whole when its parent's key is taken.
    assert_eq!(f.keys_read_whole().len(), 2);
    assert!(c.table("s.e").unwrap().keys_read_whole()[0].contains("reads s.e whole"));
    let facts: Vec<String> = c
        .facts()
        .into_iter()
        .filter(|f| f.object == "s.c.c_z_y_fkey")
        .map(|f| format!("{}={}", f.fact, f.value))
        .collect();
    assert_eq!(
        facts,
        [
            "columns=z,y",
            "deferrable=false",
            "deferred=false",
            "delete_columns=y",
            "enforced=true",
            "key=p_a_b_key",
            "match=f",
            "on_delete=n",
            "on_update=c",
            "references=s.p(b,a)",
            "type=f",
            "validated=true"
        ]
    );
    for (ddl, why) in [
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.q)", "relation \"s.q\" does not exist"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.p (d))", "cannot use a deferrable unique constraint for referenced table \"p\""),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.p (a))", "there is no unique constraint matching given keys"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer, FOREIGN KEY (x) REFERENCES s.p (a, b))", "number of referencing and referenced columns"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer, FOREIGN KEY (x, x) REFERENCES s.p (a, a))", "must not contain duplicates"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x numeric(6,2) REFERENCES s.p)", "cannot be implemented"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer, FOREIGN KEY (q) REFERENCES s.p)", "column \"q\" referenced in foreign key constraint does not exist"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer GENERATED ALWAYS AS (id) REFERENCES s.p)", "virtual generated columns are not supported"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer GENERATED ALWAYS AS (id) STORED REFERENCES s.p ON DELETE SET NULL)", "invalid ON DELETE action"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer, y integer, FOREIGN KEY (x) REFERENCES s.p ON DELETE SET NULL (y))", "must be part of foreign key"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer CONSTRAINT k REFERENCES s.p, CONSTRAINT k CHECK (x > 0))", "constraint \"k\" for relation \"c\" already exists"),
        ("CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.p NOT ENFORCED); ALTER TABLE s.c VALIDATE CONSTRAINT c_x_fkey", "cannot validate NOT ENFORCED constraint"),
        ("CREATE TABLE s.u (id integer UNIQUE); ALTER TABLE s.u CLUSTER ON u_id_key; CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.u)", "there is no primary key for referenced table \"u\""),
    ] {
        let e = refused_ddl(&format!("{PARENT} {ddl}"));
        assert!(e.contains(why), "{ddl}: {e}");
    }
    // A table whose key references a table refused is refused for it.
    let r = Catalog::read(
        "CREATE TABLE s.u (id integer UNIQUE); CREATE TABLE s.c (id integer PRIMARY KEY, x integer REFERENCES s.u (id));",
    )
    .unwrap();
    assert!(r.statements[0]
        .refused
        .as_deref()
        .unwrap()
        .contains("has no order"));
    assert!(r.statements[1]
        .refused
        .as_deref()
        .unwrap()
        .contains("references s.u, which is refused"));
}

const KEYS: &str = "CREATE TABLE s.p (id integer PRIMARY KEY, a integer, b integer, t text, u uuid, f real, UNIQUE (a, b), UNIQUE (t), UNIQUE (u), UNIQUE (f));
     CREATE TABLE s.c (id integer PRIMARY KEY, x bigint REFERENCES s.p, ya integer, yb integer, t text REFERENCES s.p (t),
         u uuid REFERENCES s.p (u), f real REFERENCES s.p (f), FOREIGN KEY (ya, yb) REFERENCES s.p (a, b));
     CREATE TABLE s.m (id integer PRIMARY KEY, ya integer, yb integer, FOREIGN KEY (ya, yb) REFERENCES s.p (a, b) MATCH FULL);";

#[test]
fn a_row_written_references_its_parent_s_key_unless_its_reference_holds_a_null() {
    let path = made("keys-reference", KEYS);
    let mut w = Snail::open_writing(&path).unwrap();
    run(
        &mut w,
        "INSERT INTO s.p (id, a, b, t, u, f) VALUES (1, 1, 1, 'one', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', 0), (2, 1, 2, 'two', NULL, 1.5)",
    );
    run(&mut w, "INSERT INTO s.c (id, x, ya, yb, t, u, f) VALUES (10, 1, 1, 2, 'one', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', '-0')");
    refused(
        &mut w,
        "INSERT INTO s.c (id, x) VALUES (11, 3)",
        "Key (x)=(3) is not present in table \"p\"",
    );
    refused(
        &mut w,
        "INSERT INTO s.c (id, t) VALUES (11, 'three')",
        "Key (t)=(three) is not present",
    );
    refused(
        &mut w,
        "INSERT INTO s.c (id, u) VALUES (11, 'b0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11')",
        "violates foreign key constraint \"c_u_fkey\"",
    );
    refused(
        &mut w,
        "INSERT INTO s.c (id, f) VALUES (11, 2.5)",
        "violates foreign key constraint \"c_f_fkey\"",
    );
    refused(
        &mut w,
        "INSERT INTO s.c (id, ya, yb) VALUES (11, 2, 1)",
        "Key (ya, yb)=(2, 1) is not present",
    );
    // Under MATCH SIMPLE a reference holding a NULL references nothing; under MATCH FULL it holds
    // only NULLs.
    run(
        &mut w,
        "INSERT INTO s.c (id, ya, yb) VALUES (11, 9, NULL), (12, NULL, NULL)",
    );
    run(
        &mut w,
        "INSERT INTO s.m (id, ya, yb) VALUES (1, 1, 2), (2, NULL, NULL)",
    );
    refused(
        &mut w,
        "INSERT INTO s.m (id, ya, yb) VALUES (3, 1, NULL)",
        "MATCH FULL does not allow mixing",
    );
    refused(
        &mut w,
        "UPDATE s.m SET yb = NULL WHERE id = 1",
        "MATCH FULL does not allow mixing",
    );
    // An update checks the reference it changed, and not one it leaves.
    run(&mut w, "UPDATE s.c SET x = 2 WHERE id = 10");
    refused(
        &mut w,
        "UPDATE s.c SET x = 4 WHERE id = 10",
        "Key (x)=(4) is not present",
    );
    run(&mut w, "UPDATE s.c SET id = 13 WHERE id = 12");
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id, x, ya, yb FROM s.c"),
        ["10,2,1,2", "11,NULL,9,NULL", "13,NULL,NULL,NULL"]
    );
}

const ACTIONS: &str = "CREATE TABLE s.p (id integer PRIMARY KEY, v integer);
     CREATE TABLE s.na (id integer PRIMARY KEY, r integer REFERENCES s.p);
     CREATE TABLE s.re (id integer PRIMARY KEY, r integer REFERENCES s.p ON DELETE RESTRICT ON UPDATE RESTRICT);
     CREATE TABLE s.ca (id integer PRIMARY KEY, r integer REFERENCES s.p ON DELETE CASCADE ON UPDATE CASCADE);
     CREATE INDEX ca_r ON s.ca (r, id);
     CREATE TABLE s.sn (id integer PRIMARY KEY, r integer, q integer, FOREIGN KEY (r, q) REFERENCES s.p2 (a, b) ON DELETE SET NULL (q) ON UPDATE SET NULL);
     CREATE TABLE s.sd (id integer PRIMARY KEY, r integer DEFAULT 1 REFERENCES s.p ON DELETE SET DEFAULT ON UPDATE SET DEFAULT);";

#[test]
fn a_key_taken_from_its_parent_is_refused_or_acted_on_in_the_same_round() {
    let path = made(
        "keys-actions",
        &ACTIONS.replace(
            "CREATE TABLE s.sn",
            "CREATE TABLE s.p2 (a integer, b integer, PRIMARY KEY (a, b)); CREATE TABLE s.sn",
        ),
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(
        &mut w,
        "INSERT INTO s.p (id, v) VALUES (1, 0), (2, 0), (3, 0), (4, 0), (5, 0), (6, 0)",
    );
    run(&mut w, "INSERT INTO s.p2 (a, b) VALUES (1, 1), (1, 2)");
    run(&mut w, "INSERT INTO s.na (id, r) VALUES (1, 2)");
    run(&mut w, "INSERT INTO s.re (id, r) VALUES (1, 3)");
    run(
        &mut w,
        "INSERT INTO s.ca (id, r) VALUES (1, 4), (2, 4), (3, 5)",
    );
    run(
        &mut w,
        "INSERT INTO s.sn (id, r, q) VALUES (1, 1, 2), (2, 1, 1)",
    );
    run(&mut w, "INSERT INTO s.sd (id, r) VALUES (1, 6), (2, 6)");
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 2",
        "Key (id)=(2) is still referenced from table \"na\"",
    );
    refused(
        &mut w,
        "UPDATE s.p SET id = 20 WHERE id = 2",
        "still referenced from table \"na\"",
    );
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 3",
        "still referenced from table \"re\"",
    );
    // A key changed only in what the parent holds beside it is no key taken.
    run(&mut w, "UPDATE s.p SET v = 9 WHERE id = 3");
    // A key whose last reference the transaction took first is taken.
    run(&mut w, "BEGIN");
    run(&mut w, "DELETE FROM s.na WHERE id = 1");
    run(&mut w, "DELETE FROM s.p WHERE id = 2");
    run(&mut w, "INSERT INTO s.na (id, r) VALUES (1, 1)");
    run(&mut w, "COMMIT");
    run(&mut w, "UPDATE s.p SET id = 40 WHERE id = 4");
    run(&mut w, "DELETE FROM s.p WHERE id = 5");
    run(&mut w, "DELETE FROM s.p2 WHERE a = 1 AND b = 2");
    run(&mut w, "UPDATE s.p2 SET b = 3 WHERE a = 1 AND b = 1");
    run(&mut w, "DELETE FROM s.p WHERE id = 6");
    // SET DEFAULT gives the default, refused where the default is the key taken, or a key the
    // parent does not hold.
    run(&mut w, "DELETE FROM s.na WHERE id = 1");
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 1",
        "still referenced from table \"sd\"",
    );
    run(&mut w, "INSERT INTO s.p (id, v) VALUES (7, 0)");
    run(&mut w, "UPDATE s.sd SET r = 7 WHERE id = 1");
    run(&mut w, "UPDATE s.sd SET r = 7 WHERE id = 2");
    run(&mut w, "DELETE FROM s.p WHERE id = 1");
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 7",
        "Key (r)=(1) is not present in table \"p\"",
    );
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(rows(&mut s, "SELECT id, r FROM s.ca"), ["1,40", "2,40"]);
    assert_eq!(
        rows(&mut s, "SELECT id, r, q FROM s.sn"),
        ["1,1,NULL", "2,NULL,NULL"]
    );
    assert_eq!(rows(&mut s, "SELECT id, r FROM s.sd"), ["1,7", "2,7"]);
    assert_eq!(rows(&mut s, "SELECT id FROM s.p"), ["3", "40", "7"]);
}

#[test]
fn a_deferred_key_is_checked_at_the_commit_and_one_that_fails_rolls_it_back() {
    let path = made(
        "keys-deferred",
        "CREATE TABLE s.p (id integer PRIMARY KEY);
         CREATE TABLE s.c (id integer PRIMARY KEY, r integer CONSTRAINT late REFERENCES s.p DEFERRABLE INITIALLY DEFERRED,
             q integer CONSTRAINT soon REFERENCES s.p DEFERRABLE, z integer CONSTRAINT now REFERENCES s.p,
             x integer CONSTRAINT strict REFERENCES s.p ON DELETE RESTRICT DEFERRABLE INITIALLY DEFERRED);",
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO s.c (id, r) VALUES (1, 7)");
    run(&mut w, "INSERT INTO s.p (id) VALUES (7)");
    run(&mut w, "COMMIT");
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO s.c (id, r) VALUES (2, 8)");
    refused(&mut w, "COMMIT", "Key (r)=(8) is not present");
    // Immediate until SET CONSTRAINTS defers it; made immediate, what was deferred is checked.
    run(&mut w, "BEGIN");
    refused(
        &mut w,
        "INSERT INTO s.c (id, q) VALUES (2, 8)",
        "constraint \"soon\"",
    );
    run(&mut w, "ROLLBACK");
    run(&mut w, "BEGIN");
    run(&mut w, "SET CONSTRAINTS soon DEFERRED");
    run(&mut w, "INSERT INTO s.c (id, q) VALUES (2, 8)");
    refused(
        &mut w,
        "SET CONSTRAINTS ALL IMMEDIATE",
        "Key (q)=(8) is not present",
    );
    run(&mut w, "ROLLBACK");
    run(&mut w, "BEGIN");
    run(&mut w, "SET CONSTRAINTS s.soon, late DEFERRED");
    run(&mut w, "INSERT INTO s.c (id, q) VALUES (3, 9)");
    run(&mut w, "INSERT INTO s.p (id) VALUES (9)");
    run(&mut w, "SET CONSTRAINTS soon IMMEDIATE");
    run(&mut w, "DELETE FROM s.p WHERE id = 7");
    run(&mut w, "INSERT INTO s.p (id) VALUES (7)");
    run(&mut w, "COMMIT");
    // RESTRICT is checked at the statement's end, deferred or not.
    run(&mut w, "INSERT INTO s.p (id) VALUES (11)");
    run(&mut w, "INSERT INTO s.c (id, x) VALUES (5, 11)");
    run(&mut w, "BEGIN");
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 11",
        "constraint \"strict\"",
    );
    run(&mut w, "ROLLBACK");
    refused(
        &mut w,
        "SET CONSTRAINTS now DEFERRED",
        "constraint \"now\" is not deferrable",
    );
    refused(
        &mut w,
        "SET CONSTRAINTS nothing DEFERRED",
        "constraint \"nothing\" does not exist",
    );
    // Outside a transaction it does nothing.
    run(&mut w, "SET CONSTRAINTS ALL DEFERRED");
    refused(
        &mut w,
        "INSERT INTO s.c (id, r) VALUES (4, 10)",
        "Key (r)=(10) is not present",
    );
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id, r, q FROM s.c"),
        ["1,7,NULL", "3,NULL,9", "5,NULL,NULL"]
    );
    assert_eq!(rows(&mut s, "SELECT id FROM s.p"), ["11", "7", "9"]);
}

#[test]
fn a_key_that_references_its_own_table_cascades_down_its_tree() {
    let path = made(
        "keys-tree",
        "CREATE TABLE s.e (id integer PRIMARY KEY, boss integer REFERENCES s.e ON DELETE CASCADE ON UPDATE CASCADE);",
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(
        &mut w,
        "INSERT INTO s.e (id, boss) VALUES (1, NULL), (2, 1), (3, 2), (4, 2), (5, NULL), (6, 5)",
    );
    refused(
        &mut w,
        "INSERT INTO s.e (id, boss) VALUES (7, 70)",
        "Key (boss)=(70) is not present",
    );
    run(&mut w, "INSERT INTO s.e (id, boss) VALUES (8, 8)");
    run(&mut w, "UPDATE s.e SET id = 50 WHERE id = 5");
    run(&mut w, "DELETE FROM s.e WHERE id = 2");
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(
        rows(&mut s, "SELECT id, boss FROM s.e"),
        ["1,NULL", "50,NULL", "6,50", "8,8"]
    );
}

#[test]
fn a_foreign_key_not_enforced_checks_and_acts_on_nothing() {
    let path = made(
        "keys-not-enforced",
        "CREATE TABLE s.p (id integer PRIMARY KEY);
         CREATE TABLE s.c (id integer PRIMARY KEY, r integer REFERENCES s.p ON DELETE CASCADE NOT ENFORCED);",
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "INSERT INTO s.p (id) VALUES (1)");
    run(&mut w, "INSERT INTO s.c (id, r) VALUES (1, 1), (2, 2)");
    run(&mut w, "DELETE FROM s.p WHERE id = 1");
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(rows(&mut s, "SELECT id, r FROM s.c"), ["1,1", "2,2"]);
}

#[test]
fn a_foreign_key_made_valid_on_rows_held_checks_every_one() {
    let path = made(
        "keys-added",
        "CREATE TABLE s.p (id integer PRIMARY KEY); CREATE TABLE s.c (id integer PRIMARY KEY, r integer);",
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "INSERT INTO s.p (id) VALUES (1)");
    run(
        &mut w,
        "INSERT INTO s.c (id, r) VALUES (1, 1), (2, 2), (3, NULL)",
    );
    refused(
        &mut w,
        "ALTER TABLE s.c ADD CONSTRAINT c_r FOREIGN KEY (r) REFERENCES s.p",
        "Key (r)=(2) is not present",
    );
    run(
        &mut w,
        "ALTER TABLE s.c ADD CONSTRAINT c_r FOREIGN KEY (r) REFERENCES s.p NOT VALID",
    );
    refused(
        &mut w,
        "INSERT INTO s.c (id, r) VALUES (4, 4)",
        "Key (r)=(4) is not present",
    );
    refused(
        &mut w,
        "ALTER TABLE s.c VALIDATE CONSTRAINT c_r",
        "Key (r)=(2) is not present",
    );
    run(&mut w, "DELETE FROM s.c WHERE id = 2");
    run(&mut w, "ALTER TABLE s.c VALIDATE CONSTRAINT c_r");
    refused(
        &mut w,
        "DELETE FROM s.p WHERE id = 1",
        "still referenced from table \"c\"",
    );
}

#[test]
fn write_refuses_an_orphan_among_the_rows_it_is_given() {
    use arrow::array::{ArrayRef, Int32Array};
    use arrow::datatypes::{DataType, Field, Schema};
    use std::sync::Arc;
    let catalog = Catalog::parse(
        "CREATE TABLE s.p (id integer PRIMARY KEY); CREATE TABLE s.c (id integer PRIMARY KEY, r integer REFERENCES s.p);",
    )
    .unwrap();
    let batch = |values: Vec<Vec<Option<i32>>>, names: &[&str]| {
        let fields: Vec<Field> = names
            .iter()
            .map(|n| Field::new(*n, DataType::Int32, true))
            .collect();
        let columns: Vec<ArrayRef> = values
            .into_iter()
            .map(|v| Arc::new(Int32Array::from(v)) as ArrayRef)
            .collect();
        RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap()
    };
    let rows_of = |name: &str, b: RecordBatch| {
        let t = catalog.table(name).unwrap();
        (
            name.to_string(),
            warren_snail::write::Rows::from_batches(t, &[b]).unwrap(),
        )
    };
    let p = || rows_of("s.p", batch(vec![vec![Some(1), Some(2)]], &["id"]));
    let c = rows_of(
        "s.c",
        batch(
            vec![
                vec![Some(1), Some(2), Some(3)],
                vec![Some(1), None, Some(5)],
            ],
            &["id", "r"],
        ),
    );
    let path = tmp("keys-write.snail");
    let e = write(&path, &catalog, &[p(), c]).unwrap_err().to_string();
    assert!(
        e.contains("Key (r)=(5) is not present in table \"p\""),
        "{e}"
    );
    assert!(!path.exists());
    let c = rows_of(
        "s.c",
        batch(
            vec![vec![Some(1), Some(2)], vec![Some(1), None]],
            &["id", "r"],
        ),
    );
    write(&path, &catalog, &[p(), c]).unwrap();
}

#[test]
fn a_snail_of_format_seven_or_eight_refuses_a_foreign_key_by_name() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.p (id integer NOT NULL, PRIMARY KEY (id)); CREATE TABLE s.c (id integer NOT NULL, r integer REFERENCES s.p, PRIMARY KEY (id));",
    )
    .unwrap();
    for n in [7, 8] {
        let path = tmp(&format!("keys-format-{n}.snail"));
        let made = match n {
            7 => warren_snail::write::write_format_seven(&path, &catalog, &[], weather::span()),
            _ => warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span()),
        };
        let e = made.unwrap_err().to_string();
        assert!(
            e.contains("the FOREIGN KEY c_r_fkey") && e.contains("`out` and `write`"),
            "{e}"
        );
    }
}

/// A writer's statements run on a thread of its own, its results sent back when they end.
fn later(
    path: &std::path::Path,
    statements: Vec<String>,
) -> mpsc::Receiver<Vec<Result<u64, String>>> {
    let (tx, rx) = mpsc::channel();
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        let done = statements
            .iter()
            .map(|st| w.execute(st).map(|d| d.rows).map_err(|e| e.to_string()))
            .collect();
        tx.send(done).unwrap();
    });
    rx
}

#[test]
fn a_child_written_and_its_parent_taken_by_two_writers_at_once_meet_at_the_parent_s_block() {
    let path = made(
        "keys-two-writers",
        "CREATE TABLE s.p (id integer PRIMARY KEY); CREATE TABLE s.c (id integer PRIMARY KEY, r integer REFERENCES s.p);",
    );
    let mut w = Snail::open_writing(&path).unwrap();
    run(&mut w, "INSERT INTO s.p (id) VALUES (1), (2)");
    // A child written first: the parent's delete waits for its commit, then is refused.
    run(&mut w, "BEGIN");
    run(&mut w, "INSERT INTO s.c (id, r) VALUES (1, 1)");
    let other = later(&path, vec!["DELETE FROM s.p WHERE id = 1".into()]);
    assert!(
        other.recv_timeout(Duration::from_millis(500)).is_err(),
        "the delete did not wait"
    );
    run(&mut w, "COMMIT");
    let done = other.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        done[0].as_ref().unwrap_err().contains("still referenced"),
        "{done:?}"
    );
    // The parent taken first: the child's check waits for its commit, then is refused.
    run(&mut w, "BEGIN");
    run(&mut w, "DELETE FROM s.p WHERE id = 2");
    let other = later(&path, vec!["INSERT INTO s.c (id, r) VALUES (2, 2)".into()]);
    assert!(
        other.recv_timeout(Duration::from_millis(500)).is_err(),
        "the insert did not wait"
    );
    run(&mut w, "COMMIT");
    let done = other.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(
        done[0]
            .as_ref()
            .unwrap_err()
            .contains("Key (r)=(2) is not present"),
        "{done:?}"
    );
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    assert_eq!(rows(&mut s, "SELECT id, r FROM s.c"), ["1,1"]);
    assert_eq!(rows(&mut s, "SELECT id FROM s.p"), ["1"]);
}
