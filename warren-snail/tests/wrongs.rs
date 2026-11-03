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
