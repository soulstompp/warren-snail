//! An aggregate whose arguments hold both DISTINCT and ORDER BY: each group's distinct values in the
//! order named, ties on the ORDER BY broken by the other arguments ascending; an ORDER BY
//! expression refused where it is not one of the arguments, a quoted literal of no type one only
//! where the aggregate leaves its argument so; and the forms PostgreSQL's parser and grammar refuse
//! around them, a window's and an ordered-set aggregate's. Every answer and refusal was given by
//! 18.6 over the same statements, in the same order.
#![cfg(feature = "standalone")]

mod weather;

use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use weather::{tmp, write};

const DDL: &str = "CREATE SCHEMA da;
     CREATE TABLE da.t (id integer NOT NULL, g text NOT NULL, o text, n integer, d text, PRIMARY KEY (id));
     CREATE TABLE da.none (id integer NOT NULL, g text NOT NULL, o text, n integer, d text, PRIMARY KEY (id));
     CREATE TABLE da.k (id integer NOT NULL, j json, p point, x xid, PRIMARY KEY (id));";

/// A snail holding eight rows in `da.t`, two in `da.k` and none in `da.none`, at a path of the
/// test's name.
fn snail(name: &str) -> Snail {
    let path = tmp(&format!("distinct-order-{name}.snail"));
    write(&path, &Catalog::parse(DDL).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO da.t VALUES (1, 'a', 'y', 2, ','), (2, 'a', 'x', 1, ';'), (3, 'a', 'y', 2, ','), \
         (4, 'b', 'z', 3, ','), (5, 'b', NULL, NULL, ','), (6, 'a', 'x', 3, ','), (7, 'b', 'z', 3, ';'), \
         (8, 'c', NULL, NULL, ',')",
    )
    .unwrap();
    w.execute("INSERT INTO da.k VALUES (1, '{}', '(1,1)', '5'), (2, '[]', '(2,2)', '6')")
        .unwrap();
    w
}

type Outcome = Result<Vec<String>, (Option<&'static str>, String)>;

/// A statement's answer, each row's values as PostgreSQL's output function writes them, `|`
/// between them; or its refusal, its SQLSTATE and its words.
fn outcome(w: &mut Snail, sql: &str) -> Outcome {
    let done = w.execute(sql).map_err(|e| (e.sqlstate(), e.to_string()))?;
    Ok(done
        .answer
        .map(|b| {
            w.text_of(&b)
                .unwrap()
                .into_iter()
                .map(|r| {
                    r.into_iter()
                        .map(|v| v.unwrap_or_else(|| "NULL".into()))
                        .collect::<Vec<_>>()
                        .join("|")
                })
                .collect()
        })
        .unwrap_or_default())
}

fn rows(r: &[&str]) -> Outcome {
    Ok(r.iter().map(|s| s.to_string()).collect())
}

fn refused(code: &'static str, words: &str) -> Outcome {
    Err((Some(code), words.into()))
}

/// 18.6's refusal of an ORDER BY expression not among a DISTINCT aggregate's arguments.
fn not_among() -> Outcome {
    refused(
        "42P10",
        "in an aggregate with DISTINCT, ORDER BY expressions must appear in argument list",
    )
}

fn unordered(ty: &str) -> Outcome {
    refused(
        "42883",
        &format!(
            "could not identify an ordering operator for type {ty}\nHINT:  Use an explicit \
             ordering operator or modify the query."
        ),
    )
}

/// Each statement answered or refused as 18.6 answers or refuses it, in its order; every one that
/// is not, named.
fn each(w: &mut Snail, asked: &[(&str, Outcome)]) {
    let wrong: Vec<String> = asked
        .iter()
        .filter_map(|(sql, want)| {
            let got = outcome(w, sql);
            (&got != want).then(|| format!("{sql}\n  got {got:?}\n  want {want:?}"))
        })
        .collect();
    assert!(
        wrong.is_empty(),
        "{} of {}:\n{}",
        wrong.len(),
        asked.len(),
        wrong.join("\n")
    );
}

#[test]
fn each_groups_distinct_values_come_in_the_order_named() {
    let mut w = snail("named");
    each(
        &mut w,
        &[
            (
                "SELECT g, string_agg(DISTINCT o, '' ORDER BY o) AS o FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|xy", "b|z", "c|NULL"]),
            ),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o) AS o FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|{x,y}", "b|{z,NULL}", "c|{NULL}"]),
            ),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o DESC) AS o FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|{y,x}", "b|{NULL,z}", "c|{NULL}"]),
            ),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o NULLS FIRST) AS o FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|{x,y}", "b|{NULL,z}", "c|{NULL}"]),
            ),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o DESC NULLS LAST) AS o FROM da.t GROUP BY g \
                 ORDER BY g",
                rows(&["a|{y,x}", "b|{z,NULL}", "c|{NULL}"]),
            ),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o ASC NULLS FIRST) AS o FROM da.t GROUP BY g \
                 ORDER BY g",
                rows(&["a|{x,y}", "b|{NULL,z}", "c|{NULL}"]),
            ),
            (
                "SELECT g, string_agg(DISTINCT o, '' ORDER BY o DESC) AS o FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|yx", "b|z", "c|NULL"]),
            ),
            // Without GROUP BY, and over no rows.
            ("SELECT array_agg(DISTINCT o ORDER BY o) FROM da.t", rows(&["{x,y,z,NULL}"])),
            ("SELECT string_agg(DISTINCT o, '-' ORDER BY o DESC) FROM da.t", rows(&["z-y-x"])),
            ("SELECT array_agg(DISTINCT o ORDER BY o) FROM da.none", rows(&["NULL"])),
            ("SELECT string_agg(DISTINCT o, '' ORDER BY o) FROM da.none", rows(&["NULL"])),
            ("SELECT string_agg(DISTINCT o, '' ORDER BY o) FROM da.t WHERE false", rows(&["NULL"])),
            ("SELECT array_agg(DISTINCT o ORDER BY o) FROM da.t WHERE g = 'c'", rows(&["{NULL}"])),
            (
                "SELECT array_agg(DISTINCT o ORDER BY o) FROM da.t WHERE id = (SELECT max(id) FROM da.none)",
                rows(&["NULL"]),
            ),
            ("SELECT array_agg(DISTINCT o ORDER BY o) FROM da.t GROUP BY ()", rows(&["{x,y,z,NULL}"])),
            // count, and the other aggregates.
            ("SELECT count(DISTINCT o ORDER BY o) FROM da.t", rows(&["3"])),
            ("SELECT count(DISTINCT o ORDER BY o) FROM da.none", rows(&["0"])),
            (
                "SELECT g, count(DISTINCT o ORDER BY o) FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|2", "b|1", "c|0"]),
            ),
            (
                "SELECT sum(DISTINCT n ORDER BY n DESC), min(DISTINCT o ORDER BY o DESC), \
                 max(DISTINCT n ORDER BY n), avg(DISTINCT n ORDER BY n) FROM da.t",
                rows(&["6|x|3|2.0000000000000000"]),
            ),
            ("SELECT json_agg(DISTINCT o ORDER BY o DESC) FROM da.t", rows(&["[null, \"z\", \"y\", \"x\"]"])),
            ("SELECT jsonb_agg(DISTINCT n ORDER BY n DESC) FROM da.t", rows(&["[null, 3, 2, 1]"])),
            ("SELECT bool_and(DISTINCT n > 1 ORDER BY n > 1) FROM da.t", rows(&["f"])),
            ("SELECT array_agg(DISTINCT n ORDER BY n DESC) FROM da.t", rows(&["{NULL,3,2,1}"])),
            (
                "SELECT g, array_agg(DISTINCT n ORDER BY n DESC) FROM da.t WHERE g <> 'c' GROUP BY g ORDER BY g",
                rows(&["a|{3,2,1}", "b|{NULL,3}"]),
            ),
            // With FILTER, in HAVING, under a subscript, over a subquery.
            ("SELECT array_agg(DISTINCT n ORDER BY n DESC) FILTER (WHERE n > 1) FROM da.t", rows(&["{3,2}"])),
            (
                "SELECT g, array_agg(DISTINCT o ORDER BY o) FILTER (WHERE id > 2) FROM da.t GROUP BY g ORDER BY g",
                rows(&["a|{x,y}", "b|{z,NULL}", "c|{NULL}"]),
            ),
            (
                "SELECT g, string_agg(DISTINCT o, '' ORDER BY o) FILTER (WHERE g <> 'a') FROM da.t GROUP BY g \
                 ORDER BY g",
                rows(&["a|NULL", "b|z", "c|NULL"]),
            ),
            (
                "SELECT g FROM da.t GROUP BY g HAVING array_length(array_agg(DISTINCT o ORDER BY o DESC), 1) > 1 \
                 ORDER BY g",
                rows(&["a", "b"]),
            ),
            (
                "SELECT string_agg(DISTINCT o, '' ORDER BY o) FROM da.t GROUP BY g HAVING count(*) > 2 ORDER BY 1",
                rows(&["xy", "z"]),
            ),
            (
                "SELECT g, (array_agg(DISTINCT o ORDER BY o DESC))[1] FROM da.t GROUP BY g ORDER BY 2 NULLS FIRST, 1",
                rows(&["b|NULL", "c|NULL", "a|y"]),
            ),
            (
                "SELECT array_agg(DISTINCT o ORDER BY o DESC) FROM (SELECT o FROM da.t WHERE id < 5) s",
                rows(&["{z,y,x}"]),
            ),
            // By an operator.
            ("SELECT string_agg(DISTINCT o, '' ORDER BY o USING ~<~) FROM da.t", rows(&["xyz"])),
            ("SELECT string_agg(DISTINCT o, '' ORDER BY o USING <) FROM da.t", rows(&["xyz"])),
            ("SELECT array_agg(DISTINCT n ORDER BY n USING <) FROM da.t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT n ORDER BY n USING >) FROM da.t", rows(&["{NULL,3,2,1}"])),
            ("SELECT array_agg(DISTINCT n ORDER BY n USING > NULLS LAST) FROM da.t", rows(&["{3,2,1,NULL}"])),
            // An item named twice counts once.
            ("SELECT array_agg(DISTINCT o ORDER BY o, o DESC) FROM da.t", rows(&["{x,y,z,NULL}"])),
            ("SELECT array_agg(DISTINCT o ORDER BY o DESC, o) FROM da.t", rows(&["{NULL,z,y,x}"])),
            // A row.
            (
                "SELECT array_agg(DISTINCT ROW(n, o) ORDER BY ROW(n, o)) FROM da.t",
                rows(&["{\"(1,x)\",\"(2,y)\",\"(3,x)\",\"(3,z)\",\"(,)\"}"]),
            ),
            (
                "SELECT array_agg(DISTINCT (n, o) ORDER BY (n, o) DESC) FROM da.t",
                rows(&["{\"(,)\",\"(3,z)\",\"(3,x)\",\"(2,y)\",\"(1,x)\"}"]),
            ),
        ],
    );
}

#[test]
fn several_arguments_are_distinct_together_the_others_breaking_ties() {
    let mut w = snail("several");
    each(
        &mut w,
        &[
            ("SELECT string_agg(DISTINCT o, d ORDER BY o) FROM da.t", rows(&["x;x,y,z;z"])),
            ("SELECT string_agg(DISTINCT o, d ORDER BY d) FROM da.t", rows(&["x,y,z;x;z"])),
            ("SELECT string_agg(DISTINCT o, d ORDER BY d DESC, o DESC) FROM da.t", rows(&["z;x,z,y,x"])),
            ("SELECT string_agg(DISTINCT o, d ORDER BY o, d) FROM da.t", rows(&["x;x,y,z;z"])),
            ("SELECT string_agg(DISTINCT o, d ORDER BY d USING ~>~) FROM da.t", rows(&["x;z,x,y,z"])),
            ("SELECT string_agg(DISTINCT o, NULL ORDER BY o) FROM da.t", rows(&["xyz"])),
            ("SELECT string_agg(DISTINCT o, ','::text ORDER BY ','::text) FROM da.t", rows(&["x,y,z"])),
            ("SELECT string_agg(DISTINCT o, ',' ORDER BY ','::text, o) FROM da.t", rows(&["x,y,z"])),
            ("SELECT corr(DISTINCT n, '1' ORDER BY '1'::float8, n) FROM da.t", rows(&["NULL"])),
            ("SELECT regr_count(DISTINCT n, '1' ORDER BY '1'::float8, n) FROM da.t", rows(&["3"])),
            ("SELECT count(DISTINCT 'a' ORDER BY 'a') FROM da.t", rows(&["1"])),
            (
                "SELECT json_object_agg(DISTINCT 'k', o ORDER BY 'k') FROM da.t",
                rows(&["{ \"k\" : \"x\", \"k\" : \"y\", \"k\" : \"z\", \"k\" : null }"]),
            ),
        ],
    );
}

#[test]
fn an_order_by_expression_is_one_of_the_arguments_or_refused() {
    let mut w = snail("among");
    each(
        &mut w,
        &[
            ("SELECT array_agg(DISTINCT o ORDER BY g) FROM da.t", not_among()),
            ("SELECT string_agg(DISTINCT o, ',' ORDER BY g) FROM da.t", not_among()),
            ("SELECT count(DISTINCT o ORDER BY g) FROM da.none", not_among()),
            ("SELECT array_agg(DISTINCT o ORDER BY 1) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT n ORDER BY n + 0) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT n ORDER BY n::int8) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT o::varchar ORDER BY o) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT lower(o) ORDER BY upper(o)) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT j ORDER BY id) FROM da.k", not_among()),
            // A collation is part of the expression.
            ("SELECT string_agg(DISTINCT o, ',' ORDER BY o COLLATE \"C\") FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT o ORDER BY o COLLATE \"C.utf8\") FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT o COLLATE \"C\" ORDER BY o) FROM da.t", not_among()),
            (
                "SELECT array_agg(DISTINCT o COLLATE \"C\" ORDER BY o COLLATE \"C\") FROM da.t",
                rows(&["{x,y,z,NULL}"]),
            ),
            // A quoted literal or NULL of no type is one only where the aggregate leaves it so.
            ("SELECT string_agg(DISTINCT o, ',' ORDER BY ',') FROM da.t", not_among()),
            ("SELECT string_agg(DISTINCT o, ',' ORDER BY ',', o) FROM da.t", not_among()),
            ("SELECT string_agg(DISTINCT o, ','::text ORDER BY ',', o) FROM da.t", not_among()),
            ("SELECT string_agg(DISTINCT o, NULL ORDER BY NULL) FROM da.t", not_among()),
            ("SELECT array_agg(DISTINCT n ORDER BY NULL) FROM da.t", not_among()),
            ("SELECT count(DISTINCT 'a' ORDER BY 'a'::text) FROM da.t", not_among()),
            ("SELECT bool_and(DISTINCT 't' ORDER BY 't') FROM da.t", not_among()),
            ("SELECT corr(DISTINCT n, '1' ORDER BY '1', n) FROM da.t", not_among()),
            ("SELECT json_object_agg(DISTINCT 'k', o ORDER BY 'k'::text) FROM da.t", not_among()),
            // The same expression however it is written.
            ("SELECT array_agg(DISTINCT o ORDER BY da.t.o) FROM da.t", rows(&["{x,y,z,NULL}"])),
            ("SELECT array_agg(DISTINCT o ORDER BY (o)) FROM da.t", rows(&["{x,y,z,NULL}"])),
            ("SELECT array_agg(DISTINCT o ORDER BY o::text) FROM da.t", rows(&["{x,y,z,NULL}"])),
            ("SELECT array_agg(DISTINCT n ORDER BY n::int4) FROM da.t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT n ORDER BY \"n\") FROM da.t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT N ORDER BY n) FROM da.t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT t.n ORDER BY n) FROM da.t t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT n ORDER BY t.n) FROM da.t t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT n + 0 ORDER BY n + 0) FROM da.t", rows(&["{1,2,3,NULL}"])),
            ("SELECT array_agg(DISTINCT upper(o) ORDER BY upper(o) DESC) FROM da.t", rows(&["{NULL,Z,Y,X}"])),
            ("SELECT array_agg(DISTINCT upper(o) ORDER BY UPPER(o) DESC) FROM da.t", rows(&["{NULL,Z,Y,X}"])),
            ("SELECT array_agg(DISTINCT 1 ORDER BY 1) FROM da.t", rows(&["{1}"])),
            ("SELECT array_agg(DISTINCT 1 ORDER BY 1 DESC) FROM da.t", rows(&["{1}"])),
            ("SELECT array_agg(DISTINCT 1.5 ORDER BY 1.5) FROM da.t", rows(&["{1.5}"])),
            ("SELECT array_agg(DISTINCT 'a'::text ORDER BY 'a'::text) FROM da.t", rows(&["{a}"])),
            ("SELECT array_agg(DISTINCT NULL::int ORDER BY NULL::int) FROM da.t", rows(&["{NULL}"])),
        ],
    );
}

#[test]
fn what_18_6_refuses_before_any_row() {
    let mut w = snail("refused");
    each(
        &mut w,
        &[
            ("SELECT array_agg(DISTINCT j ORDER BY j) FROM da.k", unordered("json")),
            ("SELECT array_agg(DISTINCT p ORDER BY p) FROM da.k", unordered("point")),
            ("SELECT array_agg(DISTINCT x ORDER BY x) FROM da.k", unordered("xid")),
            ("SELECT array_agg(DISTINCT id ORDER BY j) FROM da.k", unordered("json")),
            ("SELECT array_agg(DISTINCT j::text ORDER BY j) FROM da.k", unordered("json")),
            (
                "SELECT array_agg(DISTINCT o ORDER BY o) OVER () FROM da.t",
                refused("0A000", "DISTINCT is not implemented for window functions"),
            ),
            (
                "SELECT count(DISTINCT o ORDER BY o) OVER () FROM da.t",
                refused("0A000", "DISTINCT is not implemented for window functions"),
            ),
            (
                "SELECT percentile_cont(DISTINCT 0.5) WITHIN GROUP (ORDER BY n) FROM da.t",
                refused("42601", "cannot use DISTINCT with WITHIN GROUP"),
            ),
            (
                "SELECT percentile_cont(0.5 ORDER BY n) WITHIN GROUP (ORDER BY n) FROM da.t",
                refused("42601", "cannot use multiple ORDER BY clauses with WITHIN GROUP"),
            ),
            (
                "SELECT percentile_cont(DISTINCT 0.5 ORDER BY n) WITHIN GROUP (ORDER BY n) FROM da.t",
                refused("42601", "cannot use multiple ORDER BY clauses with WITHIN GROUP"),
            ),
            // The grammar's refusal comes before a name it cannot find.
            (
                "SELECT nosuch, percentile_cont(DISTINCT 0.5) WITHIN GROUP (ORDER BY n) FROM da.t",
                refused("42601", "cannot use DISTINCT with WITHIN GROUP"),
            ),
            ("SELECT percentile_cont(ALL 0.5) WITHIN GROUP (ORDER BY n) FROM da.t", rows(&["2.5"])),
            (
                "SELECT array_agg(o ORDER BY o) OVER () FROM da.t",
                refused("0A000", "aggregate ORDER BY is not implemented for window functions"),
            ),
        ],
    );
}
