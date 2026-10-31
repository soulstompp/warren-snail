//! The temporal keys: a PRIMARY KEY or UNIQUE whose last column is WITHOUT OVERLAPS, an EXCLUDE on
//! GiST with `=` on its other columns and `&&` on its last, an empty period refused; and a FOREIGN
//! KEY whose last column is PERIOD, a child's period held by its parent's periods together, and a
//! parent's period taken refused where it leaves a child's period within it unheld. Each read and
//! checked as PostgreSQL 18 and 19 read and check them.

mod weather;

use warren_snail::catalog::Catalog;
use weather::tmp;

/// Each statement's refusal, or None where it is taken.
fn outcomes(ddl: &str) -> Vec<Option<String>> {
    Catalog::read(ddl)
        .unwrap()
        .statements
        .into_iter()
        .map(|s| s.refused)
        .collect()
}

#[test]
fn a_key_without_overlaps_is_read_as_an_exclude_on_gist() {
    let c = Catalog::parse(
        "CREATE TABLE s.p (id int4range, valid daterange, x int, PRIMARY KEY (id, valid WITHOUT OVERLAPS),
             UNIQUE (id, valid WITHOUT OVERLAPS), UNIQUE (id, valid),
             CONSTRAINT p_n UNIQUE NULLS NOT DISTINCT (id, valid WITHOUT OVERLAPS));
         CREATE TABLE s.m (k int PRIMARY KEY, id int4multirange, valid tstzmultirange,
             UNIQUE (id, valid WITHOUT OVERLAPS) INCLUDE (k) DEFERRABLE);",
    )
    .unwrap();
    let p = c.table("s.p").unwrap();
    // The primary key is the table's order; a UNIQUE the same as it is made once, the B-tree's beside.
    let names: Vec<&str> = p.orders.iter().map(|o| o.name.as_str()).collect();
    assert_eq!(names, ["p_pkey", "p_id_valid_key", "p_n"]);
    let key = p.order("p_pkey").unwrap();
    assert!(key.table_order && key.without_overlaps() && !key.unique);
    assert_eq!(key.method.name(), "gist");
    assert_eq!(key.opclasses, ["range_ops", "range_ops"]);
    assert_eq!(key.exclusion.as_ref().unwrap().operators, ["=", "&&"]);
    assert!(p.columns.iter().take(2).all(|c| c.not_null));
    let btree = p.order("p_id_valid_key").unwrap();
    assert!(btree.unique && !btree.without_overlaps() && btree.method.name() == "btree");
    let n = p.order("p_n").unwrap();
    assert!(n.without_overlaps() && !n.nulls_distinct);
    let m = c.table("s.m").unwrap().order("m_id_valid_k_key").unwrap();
    assert_eq!(m.opclasses, ["multirange_ops", "multirange_ops"]);
    assert!(m.exclusion.as_ref().unwrap().deferrable);
    assert_eq!(m.include, [0]);
    // Its facts: a unique index, of a key on a period.
    let facts = c.facts();
    let fact = |object: &str, name: &str| {
        facts
            .iter()
            .find(|f| f.object == object && f.fact == name)
            .map(|f| f.value.clone())
    };
    assert_eq!(fact("s.p.p_pkey", "unique").as_deref(), Some("true"));
    assert_eq!(fact("s.p.p_pkey", "period").as_deref(), Some("true"));
    assert_eq!(
        fact("s.p.p_id_valid_key", "period").as_deref(),
        Some("false")
    );
}

#[test]
fn a_key_without_overlaps_is_refused_as_postgresql_refuses_it() {
    // Each refusal as PostgreSQL 18.6 and 19 RC1 gave it (28818 and 28819, 2026-10-31).
    for (key, why) in [
        (
            "PRIMARY KEY (a, r WITHOUT OVERLAPS)",
            "data type integer has no default operator class for access method \"gist\"",
        ),
        (
            "PRIMARY KEY (r, a WITHOUT OVERLAPS)",
            "column \"a\" in WITHOUT OVERLAPS is not a range or multirange type",
        ),
        (
            "UNIQUE (r WITHOUT OVERLAPS)",
            "constraint using WITHOUT OVERLAPS needs at least two columns",
        ),
        (
            "PRIMARY KEY (p, r WITHOUT OVERLAPS)",
            "could not identify an equality operator for type point",
        ),
        (
            "UNIQUE (r WITHOUT OVERLAPS, a)",
            "WITHOUT OVERLAPS is read only after the last column",
        ),
    ] {
        let got = outcomes(&format!(
            "CREATE TABLE s.k (a int, p point, r int4range, {key});"
        ));
        let e = got[0]
            .as_deref()
            .unwrap_or_else(|| panic!("{key} was taken"));
        assert!(e.contains(why), "{key}: {e}");
    }
}

const PARENTS: &str = "CREATE TABLE s.p (id int4range, valid daterange, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
    CREATE TABLE s.u (n int PRIMARY KEY, id int4range, valid daterange, UNIQUE (id, valid WITHOUT OVERLAPS));
    CREATE TABLE s.d (id int4range, valid daterange, PRIMARY KEY (id, valid WITHOUT OVERLAPS) DEFERRABLE);
    CREATE TABLE s.b (id int4range, valid daterange, PRIMARY KEY (id, valid));
    CREATE TABLE s.x (n int PRIMARY KEY, id int4range, valid daterange, EXCLUDE USING gist (id WITH =, valid WITH &&));
    CREATE TABLE s.y (n int PRIMARY KEY, id int4range, valid daterange, EXCLUDE USING gist (valid WITH &&, id WITH =));
    CREATE TABLE s.q (id int4range, b int4range, valid daterange, PRIMARY KEY (id, b, valid WITHOUT OVERLAPS));";

#[test]
fn a_foreign_key_with_period_is_read_as_postgresql_reads_it() {
    let c = Catalog::parse(&format!(
        "{PARENTS}
         CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid daterange, a int4range, b int4range,
             FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p,
             CONSTRAINT c_x FOREIGN KEY (pid, PERIOD valid) REFERENCES s.x (id, PERIOD valid) MATCH FULL,
             FOREIGN KEY (b, a, PERIOD valid) REFERENCES s.q (b, id, PERIOD valid) DEFERRABLE INITIALLY DEFERRED);"
    ))
    .unwrap();
    let t = c.table("s.c").unwrap();
    let read: Vec<_> = t
        .foreign_keys
        .iter()
        .map(|k| {
            (
                k.name.as_str(),
                k.key.as_str(),
                k.period,
                k.equal_columns().len(),
            )
        })
        .collect();
    assert_eq!(
        read,
        [
            ("c_pid_valid_fkey", "p_pkey", true, 1),
            ("c_x", "x_id_valid_excl", true, 1),
            ("c_b_a_valid_fkey", "q_pkey", true, 2),
        ]
    );
    let q = c.table("s.q").unwrap();
    assert_eq!(t.foreign_keys[2].parent_columns, [1, 0, 2]);
    assert_eq!(q.columns[t.foreign_keys[2].parent_columns[0]].name, "b");
    // Each refusal as PostgreSQL 18.6 and 19 RC1 gave it (28818 and 28819, 2026-10-31).
    for (key, why) in [
        (
            "FOREIGN KEY (pid, valid) REFERENCES s.p",
            "foreign key uses PERIOD on the referenced table but not the referencing table",
        ),
        (
            "FOREIGN KEY (pid, valid) REFERENCES s.p (id, PERIOD valid)",
            "foreign key uses PERIOD on the referenced table but not the referencing table",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p (id, valid)",
            "foreign key uses PERIOD on the referencing table but not the referenced table",
        ),
        (
            "FOREIGN KEY (pid, valid) REFERENCES s.p (id, valid)",
            "foreign key must use PERIOD when referencing a primary key using WITHOUT OVERLAPS",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.u",
            "number of referencing and referenced columns for foreign key disagree",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.d (id, PERIOD valid)",
            "cannot use a deferrable unique constraint for referenced table \"d\"",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.b",
            "could not identify an overlaps operator for foreign key",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.b (id, PERIOD valid)",
            "there is no unique constraint matching given keys for referenced table \"b\"",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.y (id, PERIOD valid)",
            "there is no unique constraint matching given keys for referenced table \"y\"",
        ),
        (
            "FOREIGN KEY (pid, PERIOD m) REFERENCES s.p (id, PERIOD valid)",
            "foreign key constraint \"k_pid_m_fkey\" cannot be implemented",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p ON DELETE CASCADE",
            "unsupported ON DELETE action for foreign key constraint using PERIOD",
        ),
        (
            "FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p ON UPDATE RESTRICT ON DELETE SET NULL",
            "unsupported ON UPDATE action for foreign key constraint using PERIOD",
        ),
    ] {
        let got = outcomes(&format!(
            "{PARENTS}
             CREATE TABLE s.k (n int PRIMARY KEY, pid int4range, valid daterange, m datemultirange, {key});"
        ));
        let e = got[7]
            .as_deref()
            .unwrap_or_else(|| panic!("{key} was taken"));
        assert!(e.contains(why), "{key}: {e}");
    }
}

#[test]
fn a_snail_of_format_eight_refuses_a_key_without_overlaps_by_name() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.p (id int4range NOT NULL, valid daterange NOT NULL, PRIMARY KEY (id, valid WITHOUT OVERLAPS));",
    )
    .unwrap();
    let path = tmp("temporal-format-8.snail");
    let e = warren_snail::write::write_format_eight(&path, &catalog, &[], weather::span())
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("the key WITHOUT OVERLAPS p_pkey") && e.contains("`out` and `write`"),
        "{e}"
    );
}

#[cfg(feature = "standalone")]
mod writes {
    use super::weather::{tmp, write};
    use arrow::array::{Array, RecordBatch};
    use std::sync::mpsc;
    use std::time::Duration;
    use warren_snail::catalog::Catalog;
    use warren_snail::read::Snail;

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

    const CONFLICT: &str = "conflicting key value violates exclusion constraint";

    #[test]
    fn a_key_without_overlaps_refuses_an_overlap_and_an_empty_period() {
        let path = made(
            "temporal-key",
            "CREATE TABLE s.p (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
             CREATE TABLE s.n (k int PRIMARY KEY, id int4range, valid int4range,
                 UNIQUE NULLS NOT DISTINCT (id, valid WITHOUT OVERLAPS));
             CREATE TABLE s.u (k int PRIMARY KEY, id int4range, valid int4range,
                 UNIQUE (id, valid WITHOUT OVERLAPS));",
        );
        let mut w = Snail::open_writing(&path).unwrap();
        // Each statement taken or refused as PostgreSQL 18.6 and 19 RC1 took or refused it (28818
        // and 28819, 2026-10-31).
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)'), ('[1,2)', '[5,10)'), ('[2,3)', '[1,5)')",
        );
        refused(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[3,7)')",
            &format!("{CONFLICT} \"p_pkey\""),
        );
        // Two rows of one key and one period, in one statement.
        refused(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[3,4)', '[1,2)'), ('[3,4)', '[1,2)')",
            &format!("{CONFLICT} \"p_pkey\""),
        );
        refused(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', 'empty')",
            "empty WITHOUT OVERLAPS value found in column \"valid\" in relation \"p\"",
        );
        refused(
            &mut w,
            "UPDATE s.p SET valid = '[4,6)' WHERE id = '[1,2)' AND valid = '[1,5)'",
            &format!("{CONFLICT} \"p_pkey\""),
        );
        run(
            &mut w,
            "UPDATE s.p SET valid = '[0,5)' WHERE id = '[1,2)' AND valid = '[1,5)'",
        );
        // A NULL conflicts with none, unless NULLS NOT DISTINCT, where it conflicts with a NULL.
        run(
            &mut w,
            "INSERT INTO s.u (k, id, valid) VALUES (1, '[1,2)', NULL), (2, '[1,2)', NULL), (3, NULL, '[1,5)'), (4, NULL, '[1,5)')",
        );
        refused(
            &mut w,
            "INSERT INTO s.u (k, id, valid) VALUES (5, NULL, 'empty')",
            "empty WITHOUT OVERLAPS value found in column \"valid\" in relation \"u\"",
        );
        run(
            &mut w,
            "INSERT INTO s.n (k, id, valid) VALUES (1, '[1,2)', NULL), (2, NULL, '[1,5)'), (3, NULL, NULL), (4, '[1,2)', '[1,5)')",
        );
        for row in [
            "(5, '[1,2)', NULL)",
            "(5, NULL, '[3,8)')",
            "(5, NULL, NULL)",
            "(5, '[1,2)', '[4,6)')",
        ] {
            refused(
                &mut w,
                &format!("INSERT INTO s.n (k, id, valid) VALUES {row}"),
                &format!("{CONFLICT} \"n_id_valid_key\""),
            );
        }
        run(
            &mut w,
            "INSERT INTO s.n (k, id, valid) VALUES (5, '[2,3)', NULL), (6, NULL, '[5,8)')",
        );
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(
            rows(&mut s, "SELECT k FROM s.n"),
            ["1", "2", "3", "4", "5", "6"]
        );
        assert_eq!(rows(&mut s, "SELECT count(*) FROM s.p"), ["3"]);
    }

    const TEMPORAL: &str = "CREATE TABLE s.p (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
        CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid int4range,
            FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p MATCH FULL);
        CREATE TABLE s.pm (id int4range, valid int4multirange, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
        CREATE TABLE s.cm (n int PRIMARY KEY, pid int4range, valid int4multirange,
            FOREIGN KEY (pid, PERIOD valid) REFERENCES s.pm);
        CREATE INDEX cm_pid ON s.cm (pid, valid);";

    fn not_present(key: &str, parent: &str) -> String {
        format!("Key {key} is not present in table \"{parent}\"")
    }

    #[test]
    fn a_child_s_period_is_held_by_its_parents_periods_together() {
        let path = made("temporal-child", TEMPORAL);
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)'), ('[1,2)', '[5,10)'), ('[2,3)', '[1,3)'), ('[2,3)', '[4,6)')",
        );
        // Each statement taken or refused as PostgreSQL 18.6 and 19 RC1 took or refused it (28818
        // and 28819, 2026-10-31).
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, '[1,2)', '[2,8)'), (2, '[2,3)', '[4,5)')",
        );
        for (row, key) in [
            ("(3, '[1,2)', '[2,11)')", "(pid, valid)=([1,2), [2,11))"),
            ("(3, '[2,3)', '[1,6)')", "(pid, valid)=([2,3), [1,6))"),
            ("(3, '[3,4)', '[1,2)')", "(pid, valid)=([3,4), [1,2))"),
            ("(3, '[1,2)', 'empty')", "(pid, valid)=([1,2), empty)"),
        ] {
            refused(
                &mut w,
                &format!("INSERT INTO s.c (n, pid, valid) VALUES {row}"),
                &not_present(key, "p"),
            );
        }
        refused(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (3, '[1,2)', NULL)",
            "MATCH FULL does not allow mixing of null and nonnull key values",
        );
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (3, NULL, NULL)",
        );
        refused(
            &mut w,
            "UPDATE s.c SET valid = '[2,11)' WHERE n = 1",
            &not_present("(pid, valid)=([1,2), [2,11))", "p"),
        );
        run(&mut w, "UPDATE s.c SET valid = '[1,10)' WHERE n = 1");
        // A multirange child, held by its parents' multiranges together.
        run(
            &mut w,
            "INSERT INTO s.pm (id, valid) VALUES ('[1,2)', '{[1,3),[5,7)}'), ('[1,2)', '{[3,5)}')",
        );
        run(
            &mut w,
            "INSERT INTO s.cm (n, pid, valid) VALUES (1, '[1,2)', '{[2,6)}'), (2, '[1,2)', '{[1,2),[6,7)}'), (3, NULL, '{[9,10)}')",
        );
        for row in ["(4, '[1,2)', '{[2,8)}')", "(4, '[1,2)', '{}')"] {
            refused(
                &mut w,
                &format!("INSERT INTO s.cm (n, pid, valid) VALUES {row}"),
                "violates foreign key constraint \"cm_pid_valid_fkey\"",
            );
        }
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(rows(&mut s, "SELECT n FROM s.c"), ["1", "2", "3"]);
        assert_eq!(rows(&mut s, "SELECT n FROM s.cm"), ["1", "2", "3"]);
    }

    #[test]
    fn a_parent_s_period_taken_is_refused_where_a_child_s_period_within_it_is_left_unheld() {
        let path = made("temporal-parent", TEMPORAL);
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)'), ('[1,2)', '[5,10)'), ('[2,3)', '[1,3)'), ('[2,3)', '[4,6)')",
        );
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, '[1,2)', '[2,8)'), (2, '[2,3)', '[4,5)')",
        );
        // Each statement taken or refused as PostgreSQL 18.6 and 19 RC1 took or refused it (28818
        // and 28819, 2026-10-31).
        let still = |key: &str| format!("Key {key} is still referenced from table \"c\"");
        for (sql, key) in [
            (
                "DELETE FROM s.p WHERE id = '[1,2)' AND valid = '[5,10)'",
                "(id, valid)=([1,2), [5,10))",
            ),
            (
                "UPDATE s.p SET valid = '[5,7)' WHERE id = '[1,2)' AND valid = '[5,10)'",
                "(id, valid)=([1,2), [5,10))",
            ),
            (
                "UPDATE s.p SET valid = '[3,5)' WHERE id = '[1,2)' AND valid = '[1,5)'",
                "(id, valid)=([1,2), [1,5))",
            ),
            (
                "UPDATE s.p SET id = '[7,8)' WHERE id = '[2,3)' AND valid = '[4,6)'",
                "(id, valid)=([2,3), [4,6))",
            ),
        ] {
            refused(&mut w, sql, &still(key));
        }
        // A child's period within the part kept, or a parent no child's period meets.
        run(
            &mut w,
            "UPDATE s.p SET valid = '[5,12)' WHERE id = '[1,2)' AND valid = '[5,10)'",
        );
        run(
            &mut w,
            "UPDATE s.p SET valid = '[2,5)' WHERE id = '[1,2)' AND valid = '[1,5)'",
        );
        run(
            &mut w,
            "DELETE FROM s.p WHERE id = '[2,3)' AND valid = '[1,3)'",
        );
        // In a transaction, a key not deferred is checked at the statement's end.
        run(&mut w, "BEGIN");
        refused(
            &mut w,
            "DELETE FROM s.p WHERE id = '[1,2)' AND valid = '[2,5)'",
            &still("(id, valid)=([1,2), [2,5))"),
        );
        run(&mut w, "ROLLBACK");
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(
            rows(
                &mut s,
                "WITH q AS (SELECT id, valid FROM s.p) SELECT id::text AS i, valid::text AS v FROM q"
            ),
            ["[1,2),[2,5)", "[1,2),[5,12)", "[2,3),[4,6)"]
        );
    }

    #[test]
    fn a_deferred_period_is_held_at_the_commit() {
        let path = made(
            "temporal-deferred",
            "CREATE TABLE s.p (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
             CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid int4range,
                 CONSTRAINT c_p FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p DEFERRABLE INITIALLY DEFERRED);",
        );
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)')",
        );
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, '[1,2)', '[2,4)')",
        );
        // Taken and given back as two rows: whole at the commit.
        run(&mut w, "BEGIN");
        run(&mut w, "DELETE FROM s.p WHERE id = '[1,2)'");
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,3)'), ('[1,2)', '[3,6)')",
        );
        run(&mut w, "COMMIT");
        // Taken in part: refused at the commit, which rolls it back.
        run(&mut w, "BEGIN");
        run(
            &mut w,
            "DELETE FROM s.p WHERE id = '[1,2)' AND valid = '[3,6)'",
        );
        refused(
            &mut w,
            "COMMIT",
            "Key (id, valid)=([1,2), [3,6)) is still referenced from table \"c\"",
        );
        // A child written before its parent, in one transaction.
        run(&mut w, "BEGIN");
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (2, '[1,2)', '[6,9)')",
        );
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[6,9)')",
        );
        run(&mut w, "COMMIT");
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(rows(&mut s, "SELECT n FROM s.c"), ["1", "2"]);
        assert_eq!(rows(&mut s, "SELECT count(*) FROM s.p"), ["3"]);
    }

    #[test]
    fn a_child_s_period_that_shrinks_is_not_checked_again() {
        let path = made(
            "temporal-shrinks",
            "CREATE TABLE s.p (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
             CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid int4range);",
        );
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)'), ('[1,2)', '[5,10)')",
        );
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, '[1,2)', '[0,20)')",
        );
        // Each statement taken or refused as PostgreSQL 18.6 and 19 RC1 took or refused it (28818
        // and 28819, 2026-10-31).
        refused(
            &mut w,
            "ALTER TABLE s.c ADD CONSTRAINT k FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p (id, PERIOD valid)",
            "Key (pid, valid)=([1,2), [0,20)) is not present in table \"p\"",
        );
        run(
            &mut w,
            "ALTER TABLE s.c ADD CONSTRAINT k FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p (id, PERIOD valid) NOT VALID",
        );
        // A period within the one it replaces is not checked; one that grows is.
        run(&mut w, "UPDATE s.c SET valid = '[0,15)' WHERE n = 1");
        refused(
            &mut w,
            "UPDATE s.c SET valid = '[0,25)' WHERE n = 1",
            "Key (pid, valid)=([1,2), [0,25)) is not present in table \"p\"",
        );
        run(&mut w, "UPDATE s.c SET valid = '[1,15)' WHERE n = 1");
        // A parent's period taken is checked only within it.
        refused(
            &mut w,
            "DELETE FROM s.p WHERE valid = '[5,10)'",
            "Key (id, valid)=([1,2), [5,10)) is still referenced from table \"c\"",
        );
        // Validated, every row is held whole, where PostgreSQL's VALIDATE CONSTRAINT asks only that
        // a parent's period meet it (ledgers/t10b2.md, reading for Kenny).
        refused(
            &mut w,
            "ALTER TABLE s.c VALIDATE CONSTRAINT k",
            "Key (pid, valid)=([1,2), [1,15)) is not present in table \"p\"",
        );
    }

    #[test]
    fn write_holds_a_child_s_period_to_its_parents_periods() {
        // A child's rows, read out of a snail as Arrow, given to `write` beside a parent with a key
        // WITHOUT OVERLAPS, whose own rows `write` leaves to INSERT: a reference holding no NULL
        // finds no period of the parent's, and one holding a NULL references nothing.
        let source = made(
            "temporal-write-source",
            "CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid int4range);",
        );
        let mut w = Snail::open_writing(&source).unwrap();
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, NULL, '[1,2)'), (2, '[1,2)', '[2,8)')",
        );
        drop(w);
        let mut s = Snail::open(&source).unwrap();
        let (b, _) = s.ask("SELECT n, pid, valid FROM s.c", None).unwrap();
        let held = Catalog::parse(
            "CREATE TABLE s.p (id int4range, valid int4range, PRIMARY KEY (id, valid WITHOUT OVERLAPS));
             CREATE TABLE s.c (n int PRIMARY KEY, pid int4range, valid int4range,
                 FOREIGN KEY (pid, PERIOD valid) REFERENCES s.p);",
        )
        .unwrap();
        let rows_of = |b: &RecordBatch| {
            let t = held.table("s.c").unwrap();
            (
                "s.c".to_string(),
                warren_snail::write::Rows::from_batches(t, std::slice::from_ref(b)).unwrap(),
            )
        };
        let target = tmp("temporal-write.snail");
        let e = write(&target, &held, &[rows_of(&b)])
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("Key (pid, valid)=([1,2), [2,8)) is not present in table \"p\""),
            "{e}"
        );
        write(&target, &held, &[rows_of(&b.slice(0, 1))]).unwrap();
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
    fn two_writers_of_one_temporal_key_and_its_children_take_turns() {
        let path = made("temporal-two-writers", TEMPORAL);
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[1,5)'), ('[2,3)', '[1,5)')",
        );
        // A period held by a writer not yet committed: a second writer's overlapping one waits for
        // it, then is refused where it committed and taken where it rolled back.
        for (end, taken) in [("ROLLBACK", true), ("COMMIT", false)] {
            run(&mut w, "BEGIN");
            run(
                &mut w,
                "INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[10,20)')",
            );
            let other = later(
                &path,
                vec!["INSERT INTO s.p (id, valid) VALUES ('[1,2)', '[15,30)')".into()],
            );
            assert!(
                other.recv_timeout(Duration::from_millis(500)).is_err(),
                "the second writer did not wait"
            );
            run(&mut w, end);
            let done = other.recv_timeout(Duration::from_secs(10)).unwrap();
            assert_eq!(done[0].is_ok(), taken, "{end}: {done:?}");
            if taken {
                run(
                    &mut w,
                    "DELETE FROM s.p WHERE id = '[1,2)' AND valid = '[15,30)'",
                );
            }
        }
        // A child written under a parent's period: the parent's delete waits for its commit, then is
        // refused.
        run(&mut w, "BEGIN");
        run(
            &mut w,
            "INSERT INTO s.c (n, pid, valid) VALUES (1, '[2,3)', '[2,4)')",
        );
        let other = later(&path, vec!["DELETE FROM s.p WHERE id = '[2,3)'".into()]);
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
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(rows(&mut s, "SELECT n FROM s.c"), ["1"]);
        assert_eq!(rows(&mut s, "SELECT count(*) FROM s.p"), ["3"]);
    }
}
