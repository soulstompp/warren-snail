//! A writer's session as PostgreSQL's: SET, RESET and SHOW of the settings the snail takes, kept and
//! put back as PostgreSQL nests them; set_config and current_setting; prepared statements; cursors;
//! COPY; and EXPLAIN, each answered alike or refused by name.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::read::Snail;
use weather::*;

/// A statement's one-row, one-column answer: its column's name and its value.
fn shown(w: &mut Snail, st: &str) -> (String, String) {
    let d = w.execute(st).unwrap();
    let b = d.answer.expect("an answer");
    (
        b.schema().field(0).name().clone(),
        cell(b.column(0).as_ref(), 0).unwrap_or_default(),
    )
}

fn show(w: &mut Snail, name: &str) -> String {
    shown(w, &format!("SHOW {name}")).1
}

fn refused(w: &mut Snail, st: &str) -> String {
    w.execute(st).unwrap_err().to_string()
}

#[test]
fn show_gives_each_setting_the_snail_answers_under_in_postgresqls_words() {
    let s = sample(500);
    let path = snail("session-show", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    for (st, column, value) in [
        ("SHOW TimeZone", "TimeZone", "UTC"),
        ("SHOW TIME ZONE", "TimeZone", "UTC"),
        ("SHOW datestyle", "DateStyle", "ISO, MDY"),
        ("SHOW IntervalStyle", "IntervalStyle", "postgres"),
        ("SHOW extra_float_digits", "extra_float_digits", "1"),
        ("SHOW bytea_output", "bytea_output", "hex"),
        (
            "SHOW standard_conforming_strings",
            "standard_conforming_strings",
            "on",
        ),
        ("SHOW lc_monetary", "lc_monetary", "C"),
        ("SHOW client_encoding", "client_encoding", "UTF8"),
        ("SHOW server_encoding", "server_encoding", "UTF8"),
        ("SHOW work_mem", "work_mem", "4MB"),
        ("SHOW statement_timeout", "statement_timeout", "0"),
        ("SHOW application_name", "application_name", ""),
        (
            "SHOW TRANSACTION ISOLATION LEVEL",
            "transaction_isolation",
            "read committed",
        ),
        (
            "SHOW default_transaction_read_only",
            "default_transaction_read_only",
            "off",
        ),
    ] {
        assert_eq!(shown(&mut w, st), (column.into(), value.into()), "{st}");
    }
    for (st, why) in [
        ("SHOW ALL", "SHOW names one setting"),
        ("SHOW search_path", "has no search path"),
        ("SHOW server_version", "the server's own setting"),
        ("SHOW shared_buffers", "the server's own setting"),
        (
            "SHOW no_such",
            "unrecognized configuration parameter \"no_such\"",
        ),
        (
            "SHOW myapp.who",
            "unrecognized configuration parameter \"myapp.who\"",
        ),
    ] {
        let e = refused(&mut w, st);
        assert!(e.contains(why), "{st}: {e}");
    }
}

#[test]
fn a_setting_set_is_kept_and_put_back_as_postgresql_nests_it() {
    let s = sample(500);
    let path = snail("session-set", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("SET work_mem = '64MB'").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "64MB");
    w.execute("SET work_mem TO 1024").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "1MB");
    w.execute("RESET work_mem").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "4MB");
    // A transaction rolled back puts its SET back; SET LOCAL lasts the transaction.
    w.execute("BEGIN").unwrap();
    w.execute("SET work_mem = '8MB'").unwrap();
    w.execute("ROLLBACK").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "4MB");
    w.execute("BEGIN").unwrap();
    w.execute("SET work_mem = '8MB'").unwrap();
    w.execute("SET LOCAL work_mem = '16MB'").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "16MB");
    w.execute("SAVEPOINT a").unwrap();
    w.execute("SET work_mem = '32MB'").unwrap();
    w.execute("SET enable_seqscan = off").unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "16MB");
    assert_eq!(show(&mut w, "enable_seqscan"), "on");
    w.execute("COMMIT").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "8MB");
    // Outside a transaction, SET LOCAL lasts no longer than its statement.
    w.execute("SET LOCAL work_mem = '2MB'").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "8MB");
    w.execute("SET application_name = 'My App'").unwrap();
    w.execute("SET myapp.who TO 'kenny'").unwrap();
    assert_eq!(show(&mut w, "application_name"), "My App");
    assert_eq!(show(&mut w, "myapp.who"), "kenny");
    w.execute("RESET ALL").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "4MB");
    assert_eq!(show(&mut w, "application_name"), "");
    // A value PostgreSQL refuses is refused in its words, and fails the transaction.
    for (st, said) in [
        (
            "SET work_mem = 'lots'",
            "invalid value for parameter \"work_mem\": \"lots\"",
        ),
        (
            "SET enable_seqscan = maybe",
            "parameter \"enable_seqscan\" requires a Boolean value",
        ),
        (
            "SET work_mem = '1MB', '2MB'",
            "SET work_mem takes only one argument",
        ),
        (
            "SET no_such = 1",
            "unrecognized configuration parameter \"no_such\"",
        ),
        (
            "SET server_encoding = 'LATIN1'",
            "parameter \"server_encoding\" cannot be changed",
        ),
    ] {
        assert_eq!(refused(&mut w, st), said, "{st}");
    }
}

#[test]
fn a_setting_the_snail_answers_under_one_value_of_is_taken_there_and_refused_elsewhere() {
    let s = sample(500);
    let path = snail("session-own", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    for st in [
        "SET statement_timeout = 0",
        "SET lock_timeout = '0'",
        "SET idle_in_transaction_session_timeout = 0",
        "SET client_encoding = 'UTF8'",
        "SET NAMES 'utf-8'",
        "SET TIME ZONE 'UTC'",
        "SET TIME ZONE DEFAULT",
        "SET datestyle = ISO, MDY",
        "SET xmloption = content",
        "SET XML OPTION CONTENT",
        "SET default_tablespace = ''",
        "SET check_function_bodies = false",
        "SET row_security = off",
    ] {
        w.execute(st).unwrap_or_else(|e| panic!("{st}: {e}"));
    }
    for (st, why) in [
        (
            "SET idle_session_timeout = '5s'",
            "the snail ends no session",
        ),
        (
            "SET client_encoding = 'LATIN1'",
            "the snail answers in UTF8",
        ),
        // TimeZone is the session's: a named zone is read only under the feature zones.
        #[cfg(not(feature = "zones"))]
        (
            "SET TIME ZONE 'Europe/Lisbon'",
            "read only under the feature zones",
        ),
        ("SET xmloption = document", "it changes how xml is read"),
        ("SET search_path = public", "has no search path"),
        ("SET SCHEMA 'public'", "has no search path"),
        ("SET shared_buffers = '1GB'", "the server's own setting"),
        ("SET ROLE NONE", "a snail has none"),
        ("SET SESSION AUTHORIZATION DEFAULT", "a snail has none"),
        ("RESET ROLE", "a snail has none"),
    ] {
        let e = refused(&mut w, st);
        assert!(e.contains(why), "{st}: {e}");
    }
}

#[test]
fn the_transactions_modes_are_settings_too() {
    let s = sample(500);
    let path = snail("session-modes", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("SET default_transaction_isolation = 'repeatable read'")
        .unwrap();
    assert_eq!(
        show(&mut w, "default_transaction_isolation"),
        "repeatable read"
    );
    w.execute("BEGIN").unwrap();
    assert_eq!(show(&mut w, "transaction_isolation"), "repeatable read");
    w.execute("SET transaction_read_only = on").unwrap();
    assert_eq!(show(&mut w, "transaction_read_only"), "on");
    w.execute("ROLLBACK").unwrap();
    w.execute("RESET ALL").unwrap();
    assert_eq!(
        show(&mut w, "default_transaction_isolation"),
        "read committed"
    );
}

#[test]
fn set_config_and_current_setting_read_and_write_the_sessions_settings() {
    let s = sample(500);
    let path = snail("session-set-config", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let one = |w: &mut Snail, q: &str| -> Option<String> {
        let d = w.execute(q).unwrap();
        cell(d.answer.unwrap().column(0).as_ref(), 0)
    };
    assert_eq!(
        one(&mut w, "SELECT set_config('work_mem', '32768', false)").as_deref(),
        Some("32MB")
    );
    assert_eq!(show(&mut w, "work_mem"), "32MB");
    assert_eq!(
        one(&mut w, "SELECT current_setting('work_mem')").as_deref(),
        Some("32MB")
    );
    assert_eq!(
        one(&mut w, "SELECT set_config('myapp.tenant', '7', false)").as_deref(),
        Some("7")
    );
    assert_eq!(
        one(&mut w, "SELECT current_setting('myapp.tenant')").as_deref(),
        Some("7")
    );
    assert_eq!(
        one(&mut w, "SELECT current_setting('myapp.none', true)"),
        None
    );
    assert_eq!(
        one(&mut w, "SELECT current_setting('TimeZone')").as_deref(),
        Some("UTC")
    );
    // LOCAL outside a transaction lasts the statement alone.
    assert_eq!(
        one(&mut w, "SELECT set_config('work_mem', '1MB', true)").as_deref(),
        Some("1MB")
    );
    assert_eq!(show(&mut w, "work_mem"), "32MB");
    assert!(w
        .execute("SELECT set_config('idle_session_timeout', '5s', false)")
        .is_err());
}

/// The first column of an answer, its values as text.
fn column0(d: &warren_snail::read::Done) -> Vec<Option<String>> {
    let b = d.answer.as_ref().expect("an answer");
    (0..b.num_rows()).map(|r| cell(b.column(0).as_ref(), r)).collect()
}

#[test]
fn a_prepared_statement_runs_with_its_parameters_typed_as_postgresql_types_them() {
    let s = sample(500);
    let path = snail("session-prepared", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let (network, station) = stations_of(&s)[0];
    w.execute("PREPARE st (integer, integer) AS SELECT s.name FROM weather.stations s WHERE s.network_id = $1 AND s.station_no = $2")
        .unwrap();
    let d = w.execute(&format!("EXECUTE st({network}, {station})")).unwrap();
    assert_eq!(d.statement, "SELECT");
    assert_eq!(column0(&d), [Some(name(network, station))]);
    // An expression is worked out and cast to the parameter's type, as an assignment is.
    let d = w
        .execute(&format!("EXECUTE st({network} + 0, {station}.4)"))
        .unwrap();
    assert_eq!(column0(&d), [Some(name(network, station))]);
    // A parameter PREPARE leaves untyped takes its column's type.
    w.execute("PREPARE ins AS INSERT INTO weather.stations (network_id, station_no, name) VALUES ($1, $2, $3)")
        .unwrap();
    assert_eq!(w.execute("EXECUTE ins(9400, 1, 'Evora')").unwrap().rows, 1);
    w.execute("PREPARE up AS UPDATE weather.stations SET name = $1 WHERE network_id = $2 AND station_no = $3")
        .unwrap();
    assert_eq!(w.execute("EXECUTE up('Beja', 9400, 1)").unwrap().rows, 1);
    let d = w.execute("EXECUTE st(9400, 1)").unwrap();
    assert_eq!(column0(&d), [Some("Beja".into())]);
    w.execute("PREPARE echo AS SELECT $1").unwrap();
    let d = w.execute("EXECUTE echo('x''y')").unwrap();
    assert_eq!(column0(&d), [Some("x'y".into())]);
    // A prepared statement outlives a rollback.
    w.execute("BEGIN").unwrap();
    w.execute("PREPARE kept AS SELECT 1").unwrap();
    w.execute("ROLLBACK").unwrap();
    w.execute("EXECUTE kept").unwrap();
    for (st, said) in [
        ("PREPARE st AS SELECT 1", "prepared statement \"st\" already exists"),
        ("EXECUTE nope", "prepared statement \"nope\" does not exist"),
        (
            "EXECUTE st(1)",
            "wrong number of parameters for prepared statement \"st\": Expected 2 parameters but got 1.",
        ),
        ("EXECUTE st('a', 1)", "invalid input syntax for type integer: \"a\""),
        ("DEALLOCATE nope", "prepared statement \"nope\" does not exist"),
    ] {
        assert_eq!(refused(&mut w, st), said, "{st}");
    }
    w.execute("DEALLOCATE PREPARE st").unwrap();
    assert!(w.execute(&format!("EXECUTE st({network}, {station})")).is_err());
    w.execute("DEALLOCATE ALL").unwrap();
    assert!(w.execute("EXECUTE ins(9400, 2, 'x')").is_err());
}

#[test]
fn a_cursor_reads_the_generation_its_declaration_read_and_moves_as_postgresqls_does() {
    let s = sample(500);
    let mut sorted = s.reading_id.clone();
    sorted.sort();
    let k = &sorted[..5];
    let ids = |v: &[usize]| -> Vec<Option<String>> { v.iter().map(|&i| Some(k[i - 1].to_string())).collect() };
    let path = snail("session-cursor", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let e = refused(&mut w, "DECLARE c CURSOR FOR SELECT 1");
    assert_eq!(e, "DECLARE CURSOR can only be used in transaction blocks");
    w.execute("BEGIN").unwrap();
    w.execute(&format!("DECLARE c SCROLL CURSOR FOR SELECT r.reading_id FROM weather.readings r WHERE r.reading_id <= {} ORDER BY r.reading_id", k[4]))
        .unwrap();
    // A reading written after the declaration is not the cursor's.
    Snail::open_writing(&path)
        .unwrap()
        .execute(&format!("DELETE FROM weather.readings WHERE reading_id = {}", k[2]))
        .unwrap();
    let mut fetch = |st: &str| column0(&w.execute(st).unwrap());
    assert_eq!(fetch("FETCH c"), ids(&[1]));
    assert_eq!(fetch("FETCH 2 FROM c"), ids(&[2, 3]));
    assert_eq!(fetch("FETCH PRIOR FROM c"), ids(&[2]));
    assert_eq!(fetch("FETCH LAST IN c"), ids(&[5]));
    assert_eq!(fetch("FETCH NEXT FROM c"), ids(&[]));
    assert_eq!(fetch("FETCH BACKWARD 2 FROM c"), ids(&[5, 4]));
    assert_eq!(fetch("FETCH ABSOLUTE -1 FROM c"), ids(&[5]));
    assert_eq!(fetch("FETCH ABSOLUTE 2 FROM c"), ids(&[2]));
    assert_eq!(fetch("FETCH RELATIVE 0 FROM c"), ids(&[2]));
    assert_eq!(fetch("FETCH RELATIVE 2 FROM c"), ids(&[4]));
    assert_eq!(fetch("FETCH FIRST FROM c"), ids(&[1]));
    assert_eq!(fetch("FETCH ALL FROM c"), ids(&[2, 3, 4, 5]));
    assert_eq!(fetch("FETCH BACKWARD ALL FROM c"), ids(&[5, 4, 3, 2, 1]));
    assert_eq!(fetch("FETCH FORWARD FROM c"), ids(&[1]));
    let d = w.execute("MOVE FORWARD 3 IN c").unwrap();
    assert_eq!((d.statement, d.rows, d.answer.is_none()), ("MOVE", 3, true));
    assert_eq!(column0(&w.execute("FETCH c").unwrap()), ids(&[5]));
    w.execute("CLOSE c").unwrap();
    assert_eq!(refused(&mut w, "FETCH c"), "cursor \"c\" does not exist");
    w.execute("ROLLBACK").unwrap();
    // NO SCROLL reads forward alone.
    w.execute("BEGIN").unwrap();
    w.execute("DECLARE f NO SCROLL CURSOR FOR SELECT r.reading_id FROM weather.readings r ORDER BY r.reading_id")
        .unwrap();
    w.execute("FETCH 2 FROM f").unwrap();
    assert_eq!(refused(&mut w, "FETCH PRIOR FROM f"), "cursor can only scan forward");
    w.execute("ROLLBACK").unwrap();
    // WITH HOLD outlives the commit; a rollback closes those the transaction declared.
    w.execute("BEGIN").unwrap();
    w.execute("DECLARE h CURSOR WITH HOLD FOR SELECT r.reading_id FROM weather.readings r ORDER BY r.reading_id")
        .unwrap();
    w.execute("DECLARE g CURSOR FOR SELECT 1").unwrap();
    w.execute("COMMIT").unwrap();
    assert_eq!(column0(&w.execute("FETCH 1 FROM h").unwrap()), ids(&[1]));
    assert!(w.execute("FETCH g").is_err());
    w.execute("BEGIN").unwrap();
    w.execute("SAVEPOINT a").unwrap();
    w.execute("DECLARE k CURSOR FOR SELECT 1").unwrap();
    w.execute("ROLLBACK TO a").unwrap();
    assert!(w.execute("FETCH k").is_err());
    w.execute("ROLLBACK").unwrap();
    assert_eq!(column0(&w.execute("FETCH 1 FROM h").unwrap()), ids(&[2]));
    w.execute("CLOSE ALL").unwrap();
}

#[test]
fn where_current_of_writes_the_row_the_cursor_stands_on() {
    let s = sample(500);
    let mut sorted = s.reading_id.clone();
    sorted.sort();
    let (a, b) = (sorted[6], sorted[7]);
    let declare = format!("DECLARE c CURSOR FOR SELECT r.feed FROM weather.readings r WHERE r.reading_id >= {a} AND r.reading_id <= {b} ORDER BY r.reading_id FOR UPDATE");
    let path = snail("session-current-of", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute("DECLARE c CURSOR FOR SELECT r.feed FROM weather.readings r WHERE r.reading_id IN (SELECT 7 UNION SELECT 8) FOR UPDATE")
        .ok();
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&declare).unwrap();
    assert_eq!(
        refused(&mut w, "UPDATE weather.readings SET feed = 'x' WHERE CURRENT OF c"),
        "cursor \"c\" is not positioned on a row"
    );
    w.execute("ROLLBACK").unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&declare).unwrap();
    w.execute("FETCH c").unwrap();
    let d = w
        .execute("UPDATE weather.readings SET feed = 'AWS Spare' WHERE CURRENT OF c")
        .unwrap();
    assert_eq!(d.rows, 1);
    w.execute("FETCH c").unwrap();
    assert_eq!(
        w.execute("DELETE FROM weather.readings WHERE CURRENT OF c")
            .unwrap()
            .rows,
        1
    );
    w.execute("COMMIT").unwrap();
    let mut r = fresh(&path);
    assert_eq!(feed_of(&mut r, a), "AWS Spare");
    assert_eq!(
        one(&r
            .ask(&format!("SELECT count(*) FROM weather.readings r WHERE r.reading_id = {b}"), None)
            .unwrap()
            .0),
        0
    );
}

#[test]
fn copy_writes_rows_out_and_reads_them_back_in_text_and_csv() {
    let path = tmp("session-copy.snail");
    let catalog = warren_snail::catalog::Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.t (k integer NOT NULL, v text, b boolean, c character(3), PRIMARY KEY (k));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.t (k, v, b, c) VALUES (1, 'plain', true, 'ab'), (2, 'tab\there\nline\\back', false, NULL), \
         (3, 'a,b \"q\"', NULL, 'xyz'), (4, '', true, ''), (5, NULL, false, 'z')",
    )
    .unwrap();
    let out = |w: &mut Snail, st: &str| String::from_utf8(w.execute(st).unwrap().copied.unwrap()).unwrap();
    assert_eq!(
        out(&mut w, "COPY s.t TO STDOUT"),
        "1\tplain\tt\tab \n2\ttab\\there\\nline\\\\back\tf\t\\N\n3\ta,b \"q\"\t\\N\txyz\n4\t\tt\t   \n5\t\\N\tf\tz  \n"
    );
    assert_eq!(
        out(&mut w, "COPY s.t (k, v) TO STDOUT WITH (FORMAT csv, HEADER)"),
        "k,v\n1,plain\n2,\"tab\there\nline\\back\"\n3,\"a,b \"\"q\"\"\"\n4,\"\"\n5,\n"
    );
    assert_eq!(
        out(&mut w, "COPY (SELECT t.k, t.k * 2 AS twice FROM s.t t WHERE t.k < 3 ORDER BY t.k) TO STDOUT WITH CSV FORCE QUOTE *"),
        "\"1\",\"2\"\n\"2\",\"4\"\n"
    );
    assert_eq!(
        out(&mut w, "COPY s.t (k, v) TO STDOUT (DELIMITER '|', NULL 'NULL', FORMAT csv, FORCE_QUOTE (v))"),
        "1|\"plain\"\n2|\"tab\there\nline\\back\"\n3|\"a,b \"\"q\"\"\"\n4|\"\"\n5|NULL\n"
    );
    // Out and back in, each format: the same rows.
    for options in ["", " WITH (FORMAT csv)", " (FORMAT csv, HEADER true, DELIMITER ';')", " (HEADER)"] {
        let text = out(&mut w, &format!("COPY s.t TO STDOUT{options}"));
        w.execute("DELETE FROM s.t").unwrap();
        let d = w
            .execute_copy(&format!("COPY s.t FROM STDIN{options}"), text.as_bytes())
            .unwrap();
        assert_eq!((d.statement, d.rows), ("COPY", 5), "{options}");
        assert_eq!(out(&mut w, &format!("COPY s.t TO STDOUT{options}")), text, "{options}");
    }
    // To a file and back.
    let file = tmp("session-copy.txt");
    w.execute(&format!("COPY s.t TO '{}'", file.display())).unwrap();
    w.execute("DELETE FROM s.t WHERE k > 1").unwrap();
    w.execute("DELETE FROM s.t WHERE k = 1").unwrap();
    assert_eq!(w.execute(&format!("COPY s.t FROM '{}'", file.display())).unwrap().rows, 5);
    // A row of the wrong width, and COPY FROM STDIN with no rows handed, refused.
    for (st, rows, said) in [
        ("COPY s.t FROM STDIN", "6\tx\n", "missing data for column \"b\""),
        ("COPY s.t (k) FROM STDIN", "6\tx\n", "extra data after last expected column (line 1)"),
        ("COPY s.t TO STDOUT (FORMAT binary, HEADER)", "", "cannot specify HEADER in BINARY mode"),
        ("COPY s.t TO STDOUT (QUOTE '\"')", "", "COPY QUOTE requires CSV mode"),
    ] {
        let e = w.execute_copy(st, rows.as_bytes()).unwrap_err().to_string();
        assert!(e.starts_with(said), "{st}: {e}");
    }
    assert!(w.execute("COPY s.t FROM STDIN").is_err());
    fresh(&path).census().unwrap();
}

#[test]
fn explain_prints_the_snails_round() {
    let s = sample(2_000);
    let path = snail("session-explain", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    let (network, station) = busiest(&s);
    let q = format!(
        "SELECT r.reading_id FROM weather.readings r WHERE r.network_id = {network} AND r.station_no = {station}"
    );
    let lines = |w: &mut Snail, st: &str| -> Vec<String> {
        let d = w.execute(st).unwrap();
        assert_eq!(d.answer.as_ref().unwrap().schema().field(0).name(), "QUERY PLAN");
        column0(&d).into_iter().map(Option::unwrap).collect()
    };
    let priced = lines(&mut w, &format!("EXPLAIN {q}"));
    assert!(priced[0].starts_with("Snail round along r_station:"), "{priced:?}");
    let p = fresh(&path).price(&q, None).unwrap();
    assert!(priced[1].contains(&format!("{} of columns predicted", p.column_pages)));
    let ran = lines(&mut w, &format!("EXPLAIN (ANALYZE, VERBOSE) {q}"));
    let (_, round) = fresh(&path).ask(&q, None).unwrap();
    assert_eq!(
        ran[0],
        format!("Snail round along r_station: {} rows", round.rows)
    );
    assert!(ran.iter().any(|l| l.starts_with("  Order r_station:")), "{ran:?}");
    assert!(ran.last().unwrap().starts_with("Execution Time: "));
    let json = lines(&mut w, &format!("EXPLAIN (FORMAT JSON) {q}"));
    assert_eq!(json.len(), 1);
    assert!(json[0].starts_with("[\"Snail round along r_station:"));
    // A write runs under ANALYZE, and is refused without it.
    assert!(w
        .execute("EXPLAIN DELETE FROM weather.stations WHERE network_id = -1")
        .is_err());
    let wrote = lines(&mut w, "EXPLAIN ANALYZE DELETE FROM weather.stations WHERE network_id = -1");
    assert_eq!(wrote[0], "Snail write: DELETE 0 rows");
    assert!(w.execute("EXPLAIN (FORMAT XML) SELECT 1").is_err());
}

#[test]
fn begin_in_a_transaction_and_an_end_outside_one_warn_and_go_on() {
    let s = sample(200);
    let path = snail("session-warn", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    for st in ["COMMIT", "ROLLBACK"] {
        let d = w.execute(st).unwrap();
        assert_eq!(
            (d.statement, d.notices),
            (st, vec!["WARNING:  there is no transaction in progress".to_string()])
        );
    }
    w.execute("BEGIN").unwrap();
    let d = w.execute("BEGIN").unwrap();
    assert_eq!(d.notices, ["WARNING:  there is already a transaction in progress"]);
    w.execute("BEGIN ISOLATION LEVEL REPEATABLE READ").unwrap();
    assert_eq!(show(&mut w, "transaction_isolation"), "repeatable read");
    w.execute("COMMIT").unwrap();
}

#[test]
fn default_text_search_config_is_the_files_and_set_config_in_a_write_stands_for_the_session() {
    let s = sample(200);
    let path = snail("session-tsearch", &s);
    let mut w = Snail::open_writing(&path).unwrap();
    assert_eq!(show(&mut w, "default_text_search_config"), "pg_catalog.simple");
    w.execute("SET default_text_search_config = 'pg_catalog.simple'").unwrap();
    assert!(w
        .execute("SET default_text_search_config = 'pg_catalog.english'")
        .unwrap_err()
        .to_string()
        .contains("refused"));
    let (network, station) = stations_of(&s)[0];
    w.execute(&format!(
        "UPDATE weather.stations SET name = set_config('myapp.last', 'renamed', false) \
         WHERE network_id = {network} AND station_no = {station}"
    ))
    .unwrap();
    assert_eq!(show(&mut w, "myapp.last"), "renamed");
    w.execute("BEGIN").unwrap();
    w.execute(&format!(
        "UPDATE weather.stations SET name = set_config('work_mem', '8MB', true) \
         WHERE network_id = {network} AND station_no = {station}"
    ))
    .unwrap();
    assert_eq!(show(&mut w, "work_mem"), "8MB");
    w.execute("COMMIT").unwrap();
    assert_eq!(show(&mut w, "work_mem"), "4MB");
}

#[test]
fn copy_writes_and_reads_the_binary_format_as_postgresql_does() {
    let path = tmp("session-copy-binary.snail");
    let catalog = warren_snail::catalog::Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.b (k integer NOT NULL, v text, n numeric(10,2), d date, at timestamptz, \
         u uuid, f double precision, ok boolean, PRIMARY KEY (k));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.b (k, v, n, d, at, u, f, ok) VALUES \
         (1, 'one', 1.5, '2000-01-02', '2000-01-01 00:00:01+00', 'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11', -2.25, true), \
         (2, NULL, -12345678.9, NULL, NULL, NULL, NULL, false)",
    )
    .unwrap();
    let out = w
        .execute("COPY s.b TO STDOUT (FORMAT binary)")
        .unwrap()
        .copied
        .unwrap();
    // The header, then row 1: eight fields, k = 1, v = "one", n = 1.50 as base-10000 digits, the
    // date and the instant one day and one second past 2000.
    let mut want = b"PGCOPY\n\xff\r\n\0\0\0\0\0\0\0\0\0".to_vec();
    want.extend([0, 8, 0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 3, b'o', b'n', b'e']);
    want.extend([0, 0, 0, 12, 0, 2, 0, 0, 0, 0, 0, 2, 0, 1, 0x13, 0x88]);
    want.extend([0, 0, 0, 4, 0, 0, 0, 1, 0, 0, 0, 8, 0, 0, 0, 0, 0, 0x0f, 0x42, 0x40]);
    assert_eq!(&out[..want.len()], &want[..]);
    assert_eq!(&out[out.len() - 2..], &[0xff, 0xff]);
    w.execute("DELETE FROM s.b").unwrap();
    assert_eq!(
        w.execute_copy("COPY s.b FROM STDIN (FORMAT binary)", &out)
            .unwrap()
            .rows,
        2
    );
    assert_eq!(
        w.execute("COPY s.b TO STDOUT (FORMAT binary)")
            .unwrap()
            .copied
            .unwrap(),
        out
    );
    assert!(w
        .execute_copy("COPY s.b FROM STDIN (FORMAT binary)", b"not a copy")
        .unwrap_err()
        .to_string()
        .contains("COPY file signature not recognized"));
}

#[test]
fn a_cursor_the_round_can_stop_early_on_reads_only_the_blocks_its_fetches_take() {
    let s = sample(20_000);
    let path = snail("session-cursor-lazy", &s);
    let q = "SELECT r.reading_id, r.feed FROM weather.readings r";
    let (whole, round) = fresh(&path).ask(q, None).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("BEGIN").unwrap();
    w.execute(&format!("DECLARE c CURSOR FOR {q}")).unwrap();
    // A reading deleted after the declaration is still the cursor's.
    let first = cell(whole.column(0).as_ref(), 0).unwrap();
    Snail::open_writing(&path)
        .unwrap()
        .execute(&format!("DELETE FROM weather.readings WHERE reading_id = {first}"))
        .unwrap();
    let one = w.execute("FETCH 1 FROM c").unwrap();
    assert!(one.pages_read > 0, "{one:?}");
    assert!(
        one.pages_read < round.column_pages_read,
        "one row read {} pages where the whole read {}",
        one.pages_read,
        round.column_pages_read
    );
    let rest = w.execute("FETCH ALL FROM c").unwrap();
    assert_eq!(rest.rows, whole.num_rows() as u64 - 1);
    let mut got = column0(&one);
    got.extend(column0(&rest));
    let want: Vec<Option<String>> = (0..whole.num_rows()).map(|r| cell(whole.column(0).as_ref(), r)).collect();
    assert_eq!(got, want);
    let back = w.execute("FETCH BACKWARD 2 FROM c").unwrap();
    assert_eq!(back.pages_read, 0);
    w.execute("COMMIT").unwrap();
    // In a transaction that wrote, the cursor reads its rows, whole.
    w.execute("BEGIN").unwrap();
    w.execute("INSERT INTO weather.stations (network_id, station_no, name) VALUES (9700, 1, 'Tavira')")
        .unwrap();
    w.execute("DECLARE d CURSOR FOR SELECT s.name FROM weather.stations s WHERE s.network_id = 9700")
        .unwrap();
    assert_eq!(column0(&w.execute("FETCH d").unwrap()), [Some("Tavira".to_string())]);
    w.execute("ROLLBACK").unwrap();
}

#[test]
fn copy_from_keeps_the_rows_its_where_admits_and_skips_those_on_error_ignore_finds_unread() {
    let path = tmp("session-copy-options.snail");
    let catalog = warren_snail::catalog::Catalog::parse(
        "CREATE SCHEMA s; CREATE TABLE s.o (k integer NOT NULL, g integer, PRIMARY KEY (k));",
    )
    .unwrap();
    write(&path, &catalog, &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let d = w
        .execute_copy("COPY s.o FROM STDIN WHERE g > 10", b"1\t5\n2\t20\n3\t30\n")
        .unwrap();
    assert_eq!(d.rows, 2);
    let d = w
        .execute_copy(
            "COPY s.o FROM STDIN (ON_ERROR ignore, LOG_VERBOSITY verbose)",
            b"4\tx\n5\t50\n",
        )
        .unwrap();
    assert_eq!(d.rows, 1);
    assert_eq!(
        d.notices,
        [
            "NOTICE:  skipping row due to data type incompatibility at line 1 for column \"g\": \"x\"",
            "NOTICE:  1 row was skipped due to data type incompatibility",
        ]
    );
    let e = w
        .execute_copy("COPY s.o FROM STDIN (ON_ERROR ignore, REJECT_LIMIT 1)", b"6\ty\n7\tz\n")
        .unwrap_err()
        .to_string();
    assert_eq!(e, "skipped more than REJECT_LIMIT (1) rows due to data type incompatibility");
    assert!(w.execute_copy("COPY s.o FROM STDIN", b"8\tq\n").is_err());
    let out = w.execute("COPY s.o TO STDOUT").unwrap().copied.unwrap();
    assert_eq!(out, b"2\t20\n3\t30\n5\t50\n");
}
