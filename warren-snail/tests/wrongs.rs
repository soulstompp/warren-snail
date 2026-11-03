//! The wrong answers found outside the census (`ledgers/census-outside.md`, *Wrong answers* and *A
//! bug, refused in words of its own*), each answered as PostgreSQL 18.6 answers it or refused by
//! name: ORDER BY … USING, TABLESAMPLE, integers in other bases, a type-prefixed literal's
//! modifier, SELECT … INTO, ISNULL and strings across a line, a row a pipeline finds in a table
//! holding a numeric. Each expected answer is PostgreSQL's, read on 18.6 (`ledgers/wrongs.md`).

mod weather;

use arrow::array::{Array, RecordBatch};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{ints, tmp, write};

/// Two tables: `s.a`, six rows keyed by id, two to each `g`; `s.t`, five, NULLs in its fourth.
const DDL: &str = "CREATE TABLE s.a (id bigint PRIMARY KEY, g integer NOT NULL, name text NOT NULL);\n\
    CREATE TABLE s.t (id integer PRIMARY KEY, n integer, e text);";

fn made(name: &str) -> std::path::PathBuf {
    let path = tmp(&format!("wrongs-{name}.snail"));
    let text = |v: &[Option<&str>]| Values::Text(v.iter().map(|s| s.map(str::to_string)).collect());
    let a = Rows {
        columns: vec![
            ints(&[1, 2, 3, 4, 5, 6]),
            ints(&[10, 10, 20, 20, 30, 30]),
            text(&[Some("one"), Some("two"), Some("three"), Some("four"), Some("five"), Some("six")]),
        ],
    };
    let t = Rows {
        columns: vec![
            ints(&[1, 2, 3, 4, 5]),
            Values::Int(vec![Some(1), Some(2), Some(3), None, Some(5)]),
            text(&[Some("Apple pie"), Some("Banana bread"), Some("cherry tart"), None, Some("apple cake")]),
        ],
    };
    write(
        &path,
        &Catalog::parse(DDL).unwrap(),
        &[("s.a".into(), a), ("s.t".into(), t)],
    )
    .unwrap();
    path
}

/// An answer's rows, each cell as arrow writes it, NULL as "NULL".
fn rows(s: &mut Snail, sql: &str) -> Vec<Vec<String>> {
    let (b, _): (RecordBatch, _) = s.ask(sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|a| match a.is_null(r) {
                    true => "NULL".to_string(),
                    false => arrow::util::display::array_value_to_string(a, r).unwrap(),
                })
                .collect()
        })
        .collect()
}

/// An answer of one column, its cells joined by commas.
fn column(s: &mut Snail, sql: &str) -> String {
    rows(s, sql).into_iter().map(|r| r.join("|")).collect::<Vec<_>>().join(",")
}

fn refused(s: &mut Snail, sql: &str) -> String {
    match s.ask(sql, None) {
        Ok(_) => panic!("{sql} was taken"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn order_by_using_sorts_as_its_operator_does_wherever_it_stands() {
    let mut s = Snail::open(&made("using")).unwrap();
    for (sql, pg) in [
        ("SELECT id FROM s.a ORDER BY id USING >", "6,5,4,3,2,1"),
        ("SELECT id FROM s.a ORDER BY id USING <", "1,2,3,4,5,6"),
        ("SELECT id FROM (SELECT id FROM s.a ORDER BY id USING > LIMIT 2) y", "6,5"),
        ("WITH x AS (SELECT id FROM s.a ORDER BY id USING > LIMIT 2) SELECT id FROM x", "6,5"),
        ("SELECT id, row_number() OVER (ORDER BY id USING >) AS r FROM s.a ORDER BY id", "1|6,2|5,3|4,4|3,5|2,6|1"),
        ("SELECT id, lag(id) OVER w AS l FROM s.a WINDOW w AS (ORDER BY id USING >) ORDER BY id", "1|2,2|3,3|4,4|5,5|6,6|NULL"),
        ("SELECT first_value(id) OVER (ORDER BY id USING >) AS r FROM s.a LIMIT 1", "6"),
        ("SELECT array_agg(id ORDER BY id USING >)::text AS x FROM s.a", "{6,5,4,3,2,1}"),
        ("SELECT string_agg(name, ',' ORDER BY id USING >) AS x FROM s.a", "six,five,four,three,two,one"),
        ("SELECT percentile_disc(0.25) WITHIN GROUP (ORDER BY id USING >) AS x FROM s.a", "5"),
        ("SELECT mode() WITHIN GROUP (ORDER BY g USING >) AS x FROM s.a", "30"),
        ("SELECT DISTINCT g FROM s.a ORDER BY g USING >", "30,20,10"),
        ("SELECT g FROM s.a UNION SELECT g FROM s.a ORDER BY g USING >", "30,20,10"),
        // `>` puts NULLs first unless told, as DESC does.
        ("SELECT n FROM s.t ORDER BY n USING >", "NULL,5,3,2,1"),
        ("SELECT n FROM s.t ORDER BY n USING > NULLS LAST", "5,3,2,1,NULL"),
        ("SELECT n FROM s.t ORDER BY n USING < NULLS FIRST", "NULL,1,2,3,5"),
        // `~<~` and `~>~` order by the bytes.
        ("SELECT e FROM s.t WHERE e IS NOT NULL ORDER BY e USING ~>~", "cherry tart,apple cake,Banana bread,Apple pie"),
        ("SELECT id FROM (SELECT e, id FROM s.t WHERE e IS NOT NULL ORDER BY e USING ~>~ LIMIT 1) y", "3"),
        ("SELECT id FROM s.a ORDER BY id USING OPERATOR(pg_catalog.>)", "6,5,4,3,2,1"),
    ] {
        assert_eq!(column(&mut s, sql), pg, "{sql}");
    }
    for (sql, pg) in [
        ("SELECT id FROM s.a ORDER BY id USING =", "operator = is not a valid ordering operator"),
        ("SELECT row_number() OVER (ORDER BY id USING <>) AS r FROM s.a", "operator <> is not a valid ordering operator"),
        ("SELECT array_agg(id ORDER BY id USING @>) AS x FROM s.a", "operator does not exist: bigint @> bigint"),
        ("SELECT id FROM s.a ORDER BY id USING ~<~", "operator does not exist: bigint ~<~ bigint"),
        ("SELECT e FROM s.t ORDER BY e USING ~<=~", "operator ~<=~ is not a valid ordering operator"),
        ("SELECT g FROM s.a UNION SELECT g FROM s.a ORDER BY g USING =", "operator = is not a valid ordering operator"),
        ("SELECT id FROM s.a ORDER BY id USING OPERATOR(public.>)", "operator does not exist: bigint public.> bigint"),
    ] {
        assert_eq!(refused(&mut s, sql), pg, "{sql}");
    }
}

#[test]
fn tablesample_takes_none_at_0_every_row_at_100_and_refuses_in_postgresql_s_words() {
    let mut s = Snail::open(&made("sample")).unwrap();
    for (sql, pg) in [
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (0)", "0"),
        ("SELECT count(*) FROM s.a TABLESAMPLE BERNOULLI (0)", "0"),
        ("SELECT count(*) FROM s.a x TABLESAMPLE SYSTEM (0)", "0"),
        ("SELECT id FROM s.a TABLESAMPLE BERNOULLI (0)", ""),
        ("SELECT a.id FROM s.a a TABLESAMPLE SYSTEM (0) JOIN s.t t TABLESAMPLE SYSTEM (0) ON t.id = a.id", ""),
        ("WITH x AS (SELECT id FROM s.a TABLESAMPLE SYSTEM (0)) SELECT count(*) FROM x", "0"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (0) WHERE id = 1", "0"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (100)", "6"),
        ("SELECT count(*) FROM s.a TABLESAMPLE pg_catalog.bernoulli (100) REPEATABLE (1)", "6"),
        ("SELECT id FROM s.a TABLESAMPLE BERNOULLI (100) WHERE g = 10 ORDER BY id", "1,2"),
        ("SELECT g, count(*) FROM s.a TABLESAMPLE BERNOULLI (100) GROUP BY g ORDER BY g", "10|2,20|2,30|2"),
        ("SELECT t.id FROM s.a a JOIN s.t t TABLESAMPLE SYSTEM (100) ON t.id = a.id ORDER BY 1", "1,2,3,4,5"),
    ] {
        assert_eq!(column(&mut s, sql), pg, "{sql}");
    }
    for (sql, pg) in [
        ("SELECT count(*) FROM s.a TABLESAMPLE foo (1)", "tablesample method foo does not exist"),
        ("SELECT count(*) FROM s.a TABLESAMPLE system_rows (10)", "tablesample method system_rows does not exist"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (101)", "sample percentage must be between 0 and 100"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (-1)", "sample percentage must be between 0 and 100"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM ('NaN')", "sample percentage must be between 0 and 100"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (NULL)", "TABLESAMPLE parameter cannot be null"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (1, 2)", "tablesample method system requires 1 argument, not 2"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM ('a')", "invalid input syntax for type real: \"a\""),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (50) REPEATABLE (NULL)", "TABLESAMPLE REPEATABLE parameter cannot be null"),
        ("SELECT count(*) FROM s.a TABLESAMPLE SYSTEM (50) REPEATABLE ('x')", "invalid input syntax for type double precision: \"x\""),
        (
            "WITH x AS (SELECT id FROM s.a) SELECT count(*) FROM x TABLESAMPLE SYSTEM (10)",
            "TABLESAMPLE clause can only be applied to tables and materialized views",
        ),
        ("SELECT count(*) FROM (SELECT id FROM s.a) y TABLESAMPLE SYSTEM (10)", "syntax error at or near \"TABLESAMPLE\""),
    ] {
        assert_eq!(refused(&mut s, sql), pg, "{sql}");
    }
}

#[test]
fn a_sample_draws_by_the_counts_and_reads_only_the_pages_of_what_it_draws() {
    let n = 16_384i64;
    let path = tmp("wrongs-drawn.snail");
    let ddl = "CREATE TABLE s.r (id bigint PRIMARY KEY, v integer NOT NULL);";
    let ids: Vec<i64> = (0..n).collect();
    let rows = Rows { columns: vec![ints(&ids), ints(&ids)] };
    write(&path, &Catalog::parse(ddl).unwrap(), &[("s.r".into(), rows)]).unwrap();
    let mut s = weather::fresh(&path);
    let asked = |s: &mut Snail, clause: &str| {
        let sql = format!("SELECT count(*) AS n, sum(v) AS v FROM s.r TABLESAMPLE {clause}");
        let (b, round) = s.ask(&sql, None).unwrap_or_else(|e| panic!("{sql}: {e}"));
        let at = |c: usize| arrow::util::display::array_value_to_string(b.column(c), 0).unwrap();
        (at(0).parse::<i64>().unwrap(), at(1), round)
    };
    let (all, _, whole) = asked(&mut s, "SYSTEM (100)");
    assert_eq!(all, n);
    // Each row on its own: about a quarter, the same rows for the same seed, others for another.
    let (b7, v7, _) = asked(&mut s, "BERNOULLI (25) REPEATABLE (7)");
    assert!((3_800..4_400).contains(&b7), "{b7}");
    assert_eq!(asked(&mut s, "BERNOULLI (25) REPEATABLE (7)").1, v7);
    assert_ne!(asked(&mut s, "BERNOULLI (25) REPEATABLE (8)").1, v7);
    assert_ne!(asked(&mut s, "BERNOULLI (50)").1, asked(&mut s, "BERNOULLI (50)").1);
    // A block at a time: whole blocks, and only their pages read.
    let (s7, _, round) = asked(&mut s, "SYSTEM (25) REPEATABLE (7)");
    assert!(s7 > 0 && s7 < n && s7 % 1_024 == 0, "{s7}");
    assert!(round.column_pages_read < whole.column_pages_read, "{round:?} against {whole:?}");
    assert_eq!(round.column_pages_read, round.column_pages_predicted, "{round:?}");
}

#[test]
fn words_are_read_as_postgresql_s_scanner_reads_them() {
    let mut s = Snail::open(&made("scanned")).unwrap();
    for (sql, pg) in [
        (
            "SELECT 0x10 AS a, 0o17 AS b, 0b101 AS c, 1_000 AS d, 0x7FFFFFFFFFFFFFFF AS e",
            "16|15|5|1000|9223372036854775807",
        ),
        ("SELECT id FROM s.a WHERE id = 0x2", "2"),
        ("SELECT id FROM s.a ORDER BY id LIMIT 0b10", "1,2"),
        ("WITH q AS (SELECT 'a'\n'b' AS x, 'c'  -- note\n 'd' AS y) SELECT * FROM q", "ab|cd"),
        ("SELECT id FROM s.t WHERE n ISNULL", "4"),
        ("SELECT id FROM s.t WHERE n NOTNULL ORDER BY id", "1,2,3,5"),
        (
            "WITH q AS (SELECT id, n ISNULL AS x, n NOTNULL AS y FROM s.t) SELECT * FROM q ORDER BY id",
            "1|false|true,2|false|true,3|false|true,4|true|false,5|false|true",
        ),
        (
            "WITH q AS (SELECT interval(3) '1.23456' AS x, interval(0) '1.5' AS y) \
             SELECT x::text AS x, y::text AS y FROM q",
            "00:00:01.235|00:00:02",
        ),
    ] {
        assert_eq!(column(&mut s, sql), pg, "{sql}");
    }
    for (sql, pg) in [
        ("SELECT 0x", "invalid hexadecimal integer at or near \"0x\""),
        ("SELECT 0b", "invalid binary integer at or near \"0b\""),
        ("SELECT 0x1g", "trailing junk after numeric literal at or near \"0x1g\""),
        ("SELECT 123abc", "trailing junk after numeric literal at or near \"123abc\""),
        ("SELECT 1__0", "trailing junk after numeric literal at or near \"1__0\""),
        ("SELECT $1abc", "trailing junk after parameter at or near \"$1abc\""),
        ("SELECT interval(3) '1.23456' second", "syntax error at or near \"second\""),
    ] {
        assert_eq!(refused(&mut s, sql), pg, "{sql}");
    }
    // Before 16 a number is its decimal digits: 15 refuses a name written against one, and 14
    // reads the name as the next word (the release notes of 15 and 16).
    for release in [14, 15] {
        let path = tmp(&format!("wrongs-scanned-{release}.snail"));
        let catalog = Catalog::parse(DDL).unwrap();
        warren_snail::write::write_released(&path, &catalog, &[], &[], weather::span(), release)
            .unwrap();
        let mut s = Snail::open(&path).unwrap();
        match release {
            14 => {
                assert_eq!(column(&mut s, "SELECT 0x10"), "0");
                assert_eq!(column(&mut s, "SELECT 1_000"), "1");
            }
            _ => {
                for n in ["0x10", "1_000"] {
                    assert_eq!(
                        refused(&mut s, &format!("SELECT {n}")),
                        format!("trailing junk after numeric literal at or near \"{n}\"")
                    );
                }
            }
        }
    }
}

#[test]
fn a_type_s_modifier_is_taken_or_refused_as_postgresql_takes_it() {
    let mut s = Snail::open(&made("typmod")).unwrap();
    for (sql, pg) in [
        ("WITH q AS (SELECT numeric(5,2) '1.234' AS x, varchar(2) 'abc' AS y) SELECT x::text AS x, y FROM q", "1.23|ab"),
        ("WITH q AS (SELECT time(1) '10:00:00.26' AS x) SELECT x::text AS x FROM q", "10:00:00.3"),
    ] {
        assert_eq!(column(&mut s, sql), pg, "{sql}");
    }
    for (sql, pg) in [
        ("SELECT int4(3) '1'", "type modifier is not allowed for type \"int4\""),
        ("SELECT '1'::int4(3)", "type modifier is not allowed for type \"int4\""),
        ("SELECT '1'::text(3)", "type modifier is not allowed for type \"text\""),
        ("SELECT '1'::inet(2)", "type modifier is not allowed for type \"inet\""),
        ("SELECT id FROM s.a WHERE id = '1'::int8(3)", "type modifier is not allowed for type \"int8\""),
        ("SELECT 1::integer(3)", "syntax error at or near \"(\""),
        ("SELECT '1'::bigint(2)", "syntax error at or near \"(\""),
        ("SELECT 'abc'::varchar(0)", "length for type varchar must be at least 1"),
        ("SELECT 'a'::varchar(10485761)", "length for type varchar cannot exceed 10485760"),
    ] {
        assert_eq!(refused(&mut s, sql), pg, "{sql}");
    }
}

#[test]
fn what_sqlparser_takes_past_postgresql_s_grammar_is_refused_in_its_words() {
    let mut s = Snail::open(&made("grammar")).unwrap();
    let near = |w: &str| format!("syntax error at or near \"{w}\"");
    for (sql, pg) in [
        ("SELECT TOP 1 id FROM s.a", near("1")),
        ("SELECT id FROM (SELECT TOP 1 id FROM s.a) y", near("1")),
        ("SELECT id FROM s.a WHERE id IN (SELECT TOP 1 id FROM s.a)", near("1")),
        ("SELECT id FROM s.a LATERAL VIEW explode(x) t AS y", near("LATERAL")),
        ("SELECT id FROM s.a CONNECT BY PRIOR id = g", near("BY")),
        ("SELECT id FROM s.a FOR XML PATH", near("XML")),
        ("SELECT id FROM s.a FOR JSON AUTO", near("JSON")),
        ("SELECT id FROM s.a FOR BROWSE", near("BROWSE")),
        ("SELECT id FROM s.a WITH (NOLOCK)", near("WITH")),
        ("SELECT x.id FROM s.a x ANTI JOIN s.t ON true", near("ANTI")),
        // A string is never a label.
        ("SELECT 'a' 'b'", near("'b'")),
        ("SELECT 'a' /* c */\n'b'", near("'b'")),
        ("SELECT id AS 'x' FROM s.a", near("'x'")),
        ("SELECT name AS 'x' FROM s.a", near("'x'")),
        ("SELECT id FROM s.a 'x'", near("'x'")),
        ("SELECT * FROM (SELECT 1 INTO q) x", "SELECT ... INTO is not allowed here".into()),
    ] {
        assert_eq!(refused(&mut s, sql), pg, "{sql}");
    }
    assert!(refused(&mut s, "SELECT id FROM s.a QUALIFY row_number() OVER () = 1")
        .starts_with("QUALIFY row_number() OVER () = 1 is not read"));
    for sql in ["SELECT id, g INTO q FROM s.a", "SELECT 1 INTO q", "SELECT id INTO TEMP q FROM s.a"] {
        assert!(refused(&mut s, sql).starts_with("SELECT … INTO is not read"), "{sql}");
    }
    // A name then a string is a literal of the type it names; SEMI and ANTI before JOIN an alias.
    assert_eq!(
        column(&mut s, "WITH q AS (SELECT id, name 'x' FROM s.a) SELECT * FROM q ORDER BY id LIMIT 2"),
        "1|x,2|x"
    );
    assert_eq!(
        column(&mut s, "SELECT semi.id FROM s.a SEMI JOIN s.t ON s.t.id = semi.id ORDER BY 1"),
        "1,2,3,4,5"
    );
}

#[test]
fn a_record_of_one_field_made_in_from_is_its_field_and_a_declared_type_is_named_alone() {
    let path = tmp("wrongs-populate.snail");
    let ddl = "CREATE TYPE s.tc AS (a integer);\nCREATE TYPE s.c2 AS (a integer, b text);\n\
               CREATE TABLE s.one (a integer PRIMARY KEY);";
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut s = Snail::open(&path).unwrap();
    for (sql, pg) in [
        ("SELECT y.a FROM jsonb_populate_record(NULL::s.one, '{\"a\":7}') AS y", "7"),
        ("SELECT * FROM jsonb_populate_record(NULL::s.one, '{\"a\":7}') AS y", "7"),
        ("SELECT * FROM jsonb_populate_recordset(NULL::s.one, '[{\"a\":7},{\"a\":8}]') AS y", "7,8"),
        ("SELECT x FROM json_populate_record(NULL::s.one, '{\"a\":7}') AS y(x)", "7"),
        (
            "SELECT y.a, n FROM jsonb_populate_recordset(NULL::s.one, '[{\"a\":7},{\"a\":8}]') \
             WITH ORDINALITY AS y(a, n)",
            "7|1,8|2",
        ),
        ("SELECT * FROM jsonb_populate_record(NULL::s.one, NULL) AS y", "NULL"),
        // A type the DDL declares and no column names.
        ("SELECT y.a FROM jsonb_populate_record(NULL::s.tc, '{\"a\":7}') AS y", "7"),
        ("SELECT y.a, y.b FROM json_populate_record(NULL::s.c2, '{\"a\":7,\"b\":\"x\"}') AS y", "7|x"),
        ("WITH q AS (SELECT ROW(7)::s.tc AS x) SELECT (x).a FROM q", "7"),
    ] {
        assert_eq!(column(&mut s, sql), pg, "{sql}");
    }
}

#[test]
fn gist_takes_inet_s_class_unnamed_only_from_release_19() {
    let gist = "no default operator class for access method \"gist\"";
    for (ddl, refused) in [
        ("CREATE INDEX n_ip ON s.n USING gist (ip);", Some("inet")),
        ("CREATE INDEX n_c ON s.n USING gist (c);", Some("cidr")),
        ("CREATE INDEX n_ip ON s.n USING gist (ip inet_ops);", None),
        ("CREATE INDEX n_ip ON s.n USING spgist (ip);", None),
        ("CREATE TABLE s.m (id integer PRIMARY KEY, ip inet, EXCLUDE USING gist (ip WITH &&));", Some("inet")),
    ] {
        let ddl = format!("CREATE TABLE s.n (id integer PRIMARY KEY, ip inet, c cidr);\n{ddl}");
        let catalog = Catalog::parse(&ddl).unwrap();
        for release in [18, 19] {
            let path = tmp("wrongs-gist.snail");
            let got = warren_snail::write::write_released(
                &path,
                &catalog,
                &[],
                &[],
                weather::span(),
                release,
            );
            match (refused, release) {
                (Some(ty), 18) => assert_eq!(
                    got.err().map(|e| e.to_string()),
                    Some(format!("data type {ty} has {gist}")),
                    "{ddl}"
                ),
                _ => assert!(got.is_ok(), "{ddl} on {release}: {:?}", got.err()),
            }
        }
    }
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;

    #[test]
    fn a_domain_is_cast_to_and_a_row_of_a_domain_s_column_keeps_its_domain() {
        let path = tmp("wrongs-domain.snail");
        let ddl = "CREATE DOMAIN s.posint AS integer CHECK (VALUE > 0);\n\
                   CREATE DOMAIN s.nn AS integer NOT NULL;\n\
                   CREATE TYPE s.rp AS (d s.posint);\n\
                   CREATE TABLE s.dt (id integer PRIMARY KEY, d s.posint, r s.rp);";
        write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.dt VALUES (1, 1, ROW(1)), (2, 5, ROW(5)), (3, NULL, NULL)")
            .unwrap();
        let dissimilar = "cannot compare dissimilar column types s.posint and integer at record column 1";
        for (sql, pg) in [
            ("WITH q AS (SELECT id::s.posint AS x FROM s.dt) SELECT * FROM q ORDER BY x", Ok("1,2,3")),
            ("SELECT '5'::s.posint AS x", Ok("5")),
            ("WITH q AS (SELECT NULL::s.posint AS x) SELECT * FROM q", Ok("NULL")),
            (
                "WITH q AS (SELECT (id - 2)::s.posint AS x FROM s.dt) SELECT * FROM q",
                Err("value for domain s.posint violates check constraint \"posint_check\""),
            ),
            ("WITH q AS (SELECT NULL::s.nn AS x) SELECT * FROM q", Err("domain s.nn does not allow null values")),
            // A row of a domain's column: its field of the domain, through the steps that pass it on.
            ("SELECT r *= ROW(d) AS i FROM s.dt WHERE id = 1", Ok("true")),
            ("SELECT id FROM s.dt WHERE r *= ROW(d) ORDER BY id", Ok("1,2")),
            ("WITH q AS (SELECT d, r FROM s.dt WHERE id = 1) SELECT r *= ROW(d) AS i FROM q", Ok("true")),
            ("WITH q AS (SELECT id::s.posint AS e, r FROM s.dt WHERE id = 1) SELECT r *= ROW(e) AS i FROM q", Ok("true")),
            ("SELECT x *= y AS i FROM (SELECT ROW(d) AS x, ROW(1::s.posint) AS y FROM s.dt WHERE id = 1) q", Ok("true")),
            ("SELECT x *= y AS i FROM (SELECT ROW(d) AS x, ROW(id) AS y FROM s.dt WHERE id = 1) q", Err(dissimilar)),
            (
                "WITH q AS (SELECT d, id FROM s.dt WHERE id = 1) \
                 SELECT x *= y AS i FROM (SELECT ROW(d) AS x, ROW(id) AS y FROM q) z",
                Err(dissimilar),
            ),
        ] {
            match pg {
                Ok(a) => assert_eq!(column(&mut w, sql), a, "{sql}"),
                Err(e) => assert_eq!(refused(&mut w, sql), e, "{sql}"),
            }
        }
    }

    #[test]
    fn select_into_makes_no_table_and_says_why() {
        let mut w = Snail::open_writing(&made("into")).unwrap();
        for sql in ["SELECT id, g INTO q FROM s.a", "SELECT id INTO UNLOGGED TABLE q FROM s.a"] {
            let e = w.execute(sql).err().unwrap().to_string();
            assert!(e.starts_with("SELECT … INTO is not read"), "{sql}: {e}");
        }
        assert!(w.catalog.table("q").is_none());
        assert_eq!(column(&mut w, "SELECT count(*) FROM s.a"), "6");
    }

    #[test]
    fn a_row_a_pipeline_finds_is_its_block_s_whatever_numeric_it_holds() {
        let path = tmp("wrongs-numeric.snail");
        let ddl = "CREATE TABLE s.b (id bigint PRIMARY KEY, v integer, w numeric(10,2), x numeric, \
                   y numeric(30,4));";
        write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute(
            "INSERT INTO s.b VALUES (1, 100, 1.50, 1.50, 1.5), (2, 200, 2.50, 2.5, 2.5), \
             (3, NULL, 'NaN', 'NaN', NULL), (4, 400, 4, 4.000, 12345678901234567890.1234)",
        )
        .unwrap();
        // Each condition read by the pipeline, which answers a numeric at a precision of its own.
        for st in [
            "DELETE FROM s.b WHERE v + 1 = 101",
            "UPDATE s.b SET v = 7 WHERE v + 1 = 201",
            "UPDATE s.b SET w = w * 2 WHERE id + 0 = 4",
            "DELETE FROM s.b WHERE x + 0 = 'NaN'",
            "UPDATE s.b SET v = 9 WHERE y + 0 > 1",
        ] {
            w.execute(st).unwrap_or_else(|e| panic!("{st}: {e}"));
        }
        assert_eq!(
            rows(
                &mut w,
                "WITH q AS (SELECT id, v, w::text AS w, x::text AS x, y::text AS y FROM s.b) \
                 SELECT * FROM q ORDER BY id"
            ),
            [
                ["2", "9", "2.50", "2.5", "2.5000"],
                ["4", "9", "8.00", "4.000", "12345678901234567890.1234"],
            ]
        );
    }
}
