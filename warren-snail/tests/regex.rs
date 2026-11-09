//! The regular expressions, by PostgreSQL's own engine: `~` and its kin, SIMILAR TO, substring by a
//! pattern and the regexp_ functions over a table's texts and patterns, back references, lookaround
//! and the match preference among them; the classes under each collation of byte order; the
//! errors in PostgreSQL's words; the two functions returning a set refused by name. Every answer is
//! the one PostgreSQL 19 gave to the same question.

mod weather;

use arrow::array::{Array, StringArray};
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{tmp, write};

/// The table's rows: each a text and a pattern; the last two NULL in one of them.
const ROWS: [(Option<&str>, Option<&str>); 9] = [
    (Some("abcabc"), Some("(a)(b)(c)\\1")),
    (Some("foobarbequebaz"), Some("(bar)(beque)")),
    (Some("xyzzy"), Some("(?<=y)z+")),
    (Some("aaa"), Some("a*?")),
    (Some("Hello World"), Some("o")),
    (Some("abcd"), Some("b(c)?(x)?")),
    (Some("ÉtÉ été"), Some("[[:upper:]]+")),
    (None, Some("a")),
    (Some("abc"), None),
];

fn snail(name: &str) -> Snail {
    snail_after(name, "")
}

/// The table's snail, its DDL after `before`.
fn snail_after(name: &str, before: &str) -> Snail {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(&format!(
        "CREATE SCHEMA s; {before} CREATE TABLE s.t (id integer NOT NULL, t text, p text, PRIMARY KEY (id));"
    ))
    .unwrap();
    let columns = vec![
        Values::Int((1..=ROWS.len() as i64).map(Some).collect()),
        Values::Text(ROWS.iter().map(|r| r.0.map(str::to_string)).collect()),
        Values::Text(ROWS.iter().map(|r| r.1.map(str::to_string)).collect()),
    ];
    write(&path, &catalog, &[("s.t".into(), Rows { columns })]).unwrap();
    Snail::open(&path).unwrap()
}

/// An expression's text over every row, in the table's order.
fn each(snail: &mut Snail, expr: &str) -> warren_snail::Result<Vec<Option<String>>> {
    let q = format!("WITH x AS (SELECT id, t, p FROM s.t) SELECT id, ({expr})::text AS v FROM x");
    let (b, _) = snail.ask(&q, None)?;
    let a = b.column(1).as_any().downcast_ref::<StringArray>().unwrap();
    Ok((0..a.len())
        .map(|r| a.is_valid(r).then(|| a.value(r).to_string()))
        .collect())
}

/// One expression of constants, as text.
fn one(snail: &mut Snail, expr: &str) -> warren_snail::Result<Option<String>> {
    let q =
        format!("WITH x AS (SELECT id FROM s.t WHERE id = 1) SELECT ({expr})::text AS v FROM x");
    let (b, _) = snail.ask(&q, None)?;
    let a = b.column(0).as_any().downcast_ref::<StringArray>().unwrap();
    Ok(a.is_valid(0).then(|| a.value(0).to_string()))
}

/// What PostgreSQL gave, a row a value, `NULL` for none.
fn gave(values: &[&str]) -> Vec<Option<String>> {
    values
        .iter()
        .map(|v| (*v != "NULL").then(|| v.to_string()))
        .collect()
}

#[test]
fn the_operators_answer_as_postgresql_s() {
    let mut s = snail("regex_operators");
    let t = || {
        gave(&[
            "true", "true", "true", "true", "true", "true", "true", "NULL", "NULL",
        ])
    };
    assert_eq!(each(&mut s, "t ~ p").unwrap(), t());
    assert_eq!(each(&mut s, "t ~* p").unwrap(), t());
    let f = gave(&[
        "false", "false", "false", "false", "false", "false", "false", "NULL", "NULL",
    ]);
    assert_eq!(each(&mut s, "t !~ p").unwrap(), f);
    assert_eq!(one(&mut s, "'É' ~* 'é'").unwrap().as_deref(), Some("true"));
    assert_eq!(
        one(&mut s, "'abc' !~* 'B'").unwrap().as_deref(),
        Some("false")
    );
    // A bpchar is matched with its blanks by the operators, without them by the functions.
    for (expr, want) in [
        ("'tab'::char(6) ~ '\\s'", "true"),
        ("'line'::char(6) ~ 'e$'", "false"),
        ("'ab'::char(4) SIMILAR TO 'ab'", "false"),
        ("regexp_like('tab'::char(6), '\\s')", "false"),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
}

#[test]
fn the_regexp_functions_answer_as_postgresql_s() {
    let mut s = snail("regex_functions");
    let cases: [(&str, [&str; 9]); 8] = [
        (
            "regexp_match(t, p)",
            [
                "{a,b,c}",
                "{bar,beque}",
                "{zz}",
                "{\"\"}",
                "{o}",
                "{c,NULL}",
                "{É}",
                "NULL",
                "NULL",
            ],
        ),
        (
            "regexp_replace(t, p, '<\\&>', 'g')",
            [
                "<abca>bc",
                "foo<barbeque>baz",
                "xy<zz>y",
                "<>a<>a<>a<>",
                "Hell<o> W<o>rld",
                "a<bc>d",
                "<É>t<É> été",
                "NULL",
                "NULL",
            ],
        ),
        (
            "regexp_count(t, p)",
            ["1", "1", "1", "4", "2", "1", "2", "NULL", "NULL"],
        ),
        (
            "regexp_instr(t, p, 1, 1, 1)",
            ["5", "12", "5", "1", "6", "4", "2", "NULL", "NULL"],
        ),
        (
            "regexp_substr(t, p, 1, 1, '', 1)",
            ["a", "bar", "zz", "", "o", "c", "É", "NULL", "NULL"],
        ),
        (
            "regexp_split_to_array(t, p)",
            [
                "{\"\",bc}",
                "{foo,baz}",
                "{xy,y}",
                "{a,a,a}",
                "{Hell,\" W\",rld}",
                "{a,d}",
                "{\"\",t,\" été\"}",
                "NULL",
                "NULL",
            ],
        ),
        (
            "substring(t from p)",
            ["a", "bar", "zz", "", "o", "c", "É", "NULL", "NULL"],
        ),
        (
            "regexp_like(t, p, 'i')",
            [
                "true", "true", "true", "true", "true", "true", "true", "NULL", "NULL",
            ],
        ),
    ];
    for (expr, want) in cases {
        assert_eq!(each(&mut s, expr).unwrap(), gave(&want), "{expr}");
    }
}

#[test]
fn similar_to_reads_its_pattern_as_postgresql_translates_it() {
    let mut s = snail("regex_similar");
    for (expr, want) in [
        ("'abcabc' SIMILAR TO '(abc)+'", "true"),
        ("'abc' NOT SIMILAR TO '%b%'", "false"),
        ("'a%c' SIMILAR TO 'a#%c' ESCAPE '#'", "true"),
        (
            "substring('foobar' similar '%#\"o_b#\"%' escape '#')",
            "oob",
        ),
        ("similar_to_escape('a%_b')", "^(?:a.*.b)$"),
        ("similar_to_escape('a#%', '#')", "^(?:a\\%)$"),
        ("similar_escape('x\\%', NULL)", "^(?:x\\%)$"),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
    assert_eq!(
        one(&mut s, "substring('foobar' from '#\"o_b#\"%' for '#')").unwrap(),
        None
    );
}

#[test]
fn the_classes_are_the_collation_s() {
    let mut s = snail("regex_classes");
    for (expr, want) in [
        ("'é' ~ '[[:alpha:]]'", "true"),
        ("'é' COLLATE \"C\" ~ '[[:alpha:]]'", "false"),
        ("'ᔀ' COLLATE pg_c_utf8 ~ '[[:alpha:]]'", "true"),
        ("'٣' COLLATE pg_c_utf8 ~ '[[:digit:]]'", "false"),
        ("'٣' COLLATE pg_unicode_fast ~ '[[:digit:]]'", "true"),
        ("'É' COLLATE \"C\" ~* 'é'", "false"),
        ("'$' COLLATE pg_c_utf8 ~ '[[:punct:]]'", "true"),
        ("'$' COLLATE pg_unicode_fast ~ '[[:punct:]]'", "false"),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
}

#[test]
fn the_errors_are_postgresql_s_words() {
    let mut s = snail("regex_errors");
    for (expr, words) in [
        (
            "'abc' ~ '('",
            "invalid regular expression: parentheses () not balanced",
        ),
        (
            "'abc' ~ 'a{3,2}'",
            "invalid regular expression: invalid repetition count(s)",
        ),
        (
            "'abc' ~ '\\9'",
            "invalid regular expression: invalid backreference number",
        ),
        (
            "regexp_match('abc', 'b', 'g')",
            "regexp_match() does not support the \"global\" option",
        ),
        (
            "regexp_match('abc', 'b', 'z')",
            "invalid regular expression option: \"z\"",
        ),
        (
            "regexp_replace('abc', 'b', 'x', '2')",
            "invalid regular expression option: \"2\"",
        ),
        (
            "regexp_instr('abc', 'b', 0)",
            "invalid value for parameter \"start\": 0",
        ),
        ("'abc' SIMILAR TO 'a' ESCAPE 'xy'", "invalid escape string"),
        (
            "substring('abc' similar 'a#\"b#\"c#\"' escape '#')",
            "SQL regular expression may not contain more than two escape-double-quote separators",
        ),
    ] {
        let e = one(&mut s, expr).unwrap_err().to_string();
        assert!(e.contains(words), "{expr}: {e}");
    }
}

#[test]
fn a_function_returning_a_set_is_read_in_a_select_list_alone() {
    let mut s = snail("regex_sets");
    // Each one's first row, as PostgreSQL 19 gives it.
    for (f, first) in [
        ("regexp_matches('abc', 'b')", "{b}"),
        ("regexp_split_to_table('a b', ' ')", "a"),
    ] {
        assert_eq!(one(&mut s, f).unwrap().as_deref(), Some(first), "{f}");
    }
    let e = one(&mut s, "CASE WHEN true THEN regexp_matches('abc', 'b') END")
        .unwrap_err()
        .to_string();
    assert!(e.contains("not allowed in CASE"), "{e}");
}

#[test]
fn a_question_the_question_path_does_not_read_is_handed_over() {
    let mut s = snail("regex_question");
    let (b, _) = s
        .ask(
            "SELECT id FROM s.t WHERE t ~ 'b' AND t SIMILAR TO '%c%'",
            None,
        )
        .unwrap();
    let ids = b
        .column(0)
        .as_any()
        .downcast_ref::<arrow::array::Int64Array>();
    let ids: Vec<i64> = match ids {
        Some(a) => a.values().to_vec(),
        None => b
            .column(0)
            .as_any()
            .downcast_ref::<arrow::array::Int32Array>()
            .unwrap()
            .values()
            .iter()
            .map(|&v| i64::from(v))
            .collect(),
    };
    assert_eq!(ids, vec![1, 6, 9]);
}

#[test]
fn regexp_replace_reads_its_start_and_its_count() {
    let mut s = snail("regex_replace");
    for (expr, want) in [
        ("regexp_replace('abcabc', 'b', 'X', 3)", "abcaXc"),
        ("regexp_replace('abcabc', 'b', 'X', 1, 2)", "abcaXc"),
        ("regexp_replace('abcabc', 'B', 'X', 1, 0, 'i')", "aXcaXc"),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
}

#[test]
fn a_question_on_similar_to_alone_is_handed_over() {
    let mut s = snail("regex_similar_question");
    let (b, _) = s
        .ask("SELECT id FROM s.t WHERE t SIMILAR TO '%(z|o)%'", None)
        .unwrap();
    let ids: Vec<String> = (0..b.num_rows())
        .map(|r| arrow::util::display::array_value_to_string(b.column(0), r).unwrap())
        .collect();
    assert_eq!(ids, ["2", "3", "5"]);
}

#[test]
fn back_references_lookaround_and_the_preference_are_postgresql_s() {
    let mut s = snail("regex_preference");
    for (expr, want) in [
        // A back reference under REG_ICASE compares in lower case.
        ("regexp_match('aA', '(a)\\1', 'i')", "{a}"),
        // The first quantifier's preference rules the match: shortest, then longest.
        ("regexp_match('xabcabcy', '(a.*?)c')", "{ab}"),
        ("regexp_match('xabcabcy', '(a.*)c')", "{abcab}"),
        // A capture inside an iteration keeps its last turn's match, none where that turn missed it.
        ("regexp_match('abab', '(?:(a)|b)+')", "{NULL}"),
        ("regexp_match('foo123bar', '(?<=o)\\d+(?=b)')", "{123}"),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
}

#[cfg(feature = "collations")]
#[test]
fn the_classes_and_case_under_a_library_s_collation_are_its_library_s() {
    let mut s = snail_after(
        "regex_library",
        "CREATE COLLATION s.ci (provider = icu, locale = 'und-u-ks-level2', deterministic = false); \
         CREATE COLLATION s.cx (provider = libc, lc_collate = 'en_US.utf8', lc_ctype = 'C');",
    );
    for (expr, want) in [
        // ICU's u_isupper holds of Lu alone; glibc's iswupper_l of a title case letter too.
        ("'ǅ' COLLATE \"en-x-icu\" ~ '[[:upper:]]'", "false"),
        ("'ǅ' COLLATE \"en_US\" ~ '[[:upper:]]'", "true"),
        ("'é' COLLATE \"en_US\" ~ '^[[:upper:]]$'", "false"),
        ("'٣' COLLATE \"en-x-icu\" ~ '^[[:digit:]]$'", "true"),
        ("'٣' COLLATE \"en_US\" ~ '^[[:digit:]]$'", "false"),
        ("'\u{2028}' COLLATE \"en-x-icu\" ~ '[[:space:]]'", "true"),
        // An LC_CTYPE of C reads ASCII's classes and case.
        ("'é' COLLATE s.cx ~ '[[:alpha:]]'", "false"),
        ("'É' COLLATE s.cx ~* 'é'", "false"),
        ("'É' COLLATE \"tr-x-icu\" ~* 'é'", "true"),
        ("'É' COLLATE \"en_US\" ~* 'é'", "true"),
        ("'ſ' COLLATE \"en-x-icu\" ~* 's'", "false"),
        (
            "regexp_like('Ab' COLLATE \"en-x-icu\", '[[:lower:]]+$')",
            "true",
        ),
        (
            "regexp_replace('ΣΑΣ' COLLATE \"el-x-icu\", 'σ', 'x', 'gi')",
            "xΑx",
        ),
    ] {
        assert_eq!(one(&mut s, expr).unwrap().as_deref(), Some(want), "{expr}");
    }
    for expr in [
        "'a' COLLATE s.ci ~ 'a'",
        "regexp_match('a' COLLATE s.ci, 'a')",
    ] {
        let e = one(&mut s, expr).unwrap_err().to_string();
        assert!(
            e.contains("nondeterministic collations are not supported for regular expressions"),
            "{expr}: {e}"
        );
    }
    assert_eq!(
        one(&mut s, "(NULL::text) COLLATE s.ci ~ 'a'").unwrap(),
        None
    );
}
