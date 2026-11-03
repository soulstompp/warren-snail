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
            "SHOW myapp.user",
            "unrecognized configuration parameter \"myapp.user\"",
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
    w.execute("SET myapp.user TO 'kenny'").unwrap();
    assert_eq!(show(&mut w, "application_name"), "My App");
    assert_eq!(show(&mut w, "myapp.user"), "kenny");
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
            "SET statement_timeout = '5s'",
            "the snail never cancels a statement",
        ),
        (
            "SET client_encoding = 'LATIN1'",
            "the snail answers in UTF8",
        ),
        (
            "SET TIME ZONE 'Europe/Lisbon'",
            "the snail answers under timezone UTC",
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
        .execute("SELECT set_config('statement_timeout', '5s', false)")
        .is_err());
}
