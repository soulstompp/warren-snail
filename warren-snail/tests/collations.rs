//! Text under a collation that calls a library, built with the feature `collations`: keyed, cut,
//! compared, ordered and grouped as the collation orders it, ICU's and glibc's, nondeterministic
//! among them; each order recording the collation and version it was cut under, and refused by
//! name under another until re-cut.

#![cfg(feature = "collations")]

mod weather;

use arrow::array::{RecordBatch, StringArray};
use std::cmp::Ordering;
use warren_snail::catalog::Catalog;
use warren_snail::collations::{open, Spec};
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{ints64, one, tmp, write};

/// The 40 strings of `types/collations.md` §6, by id from 1.
const FORTY: [&str; 40] = [
    "", " a", "a", "A", "b", "B", "z", "Z", "á", "Á", "a\u{301}", "ä", "ß", "ss", "SS", "é", "e",
    "ø", "\u{fb01}", "ΣΑΣ", "a b", "ab", "a-b", "a_b", "Ab", "!", "1", "10", "2", "\u{663}", "α",
    "Ω", "д", "Д", "א", "中", "あ", "\u{130}", "\u{1c6}", "😀",
];

fn snail_of(name: &str, ddl: &str, values: &[&str]) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(ddl).unwrap();
    let ids = Values::Int((1..=values.len() as i64).map(Some).collect());
    let v = Values::Text(values.iter().map(|s| Some(s.to_string())).collect());
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![ids, v],
            },
        )],
    )
    .unwrap();
    path
}

fn ids(b: &RecordBatch) -> Vec<i64> {
    if b.num_rows() == 0 {
        return Vec::new();
    }
    ints64(b.column(0).as_ref()).into_iter().flatten().collect()
}

fn texts(b: &RecordBatch, c: usize) -> Vec<String> {
    let a = b.column(c).as_any().downcast_ref::<StringArray>().unwrap();
    a.iter()
        .map(|v| v.unwrap_or_default().to_string())
        .collect()
}

/// The ids of `values` in the order `spec` sorts them, the id breaking ties.
fn sorted(spec: &Spec, values: &[&str]) -> Vec<i64> {
    let (_, c) = open(spec, "test").unwrap();
    let mut ids: Vec<i64> = (1..=values.len() as i64).collect();
    ids.sort_by(|&a, &b| {
        match c.cmp(
            values[a as usize - 1].as_bytes(),
            values[b as usize - 1].as_bytes(),
        ) {
            Ordering::Equal => a.cmp(&b),
            o => o,
        }
    });
    ids
}

const ICU: &str = "CREATE TABLE s.t (id integer NOT NULL, v text COLLATE \"en-US-x-icu\", \
                   PRIMARY KEY (id)); CREATE INDEX t_v ON s.t (v, id);";

#[test]
fn text_under_an_icu_collation_is_ordered_as_icu_orders_it_along_every_order() {
    let path = snail_of("icu-forty", ICU, &FORTY);
    let want = sorted(&Spec::icu("en-US", None, true), &FORTY);
    let mut snail = Snail::open(&path).unwrap();
    for order in [None, Some("t_v"), Some("t_pkey")] {
        let (b, _) = snail
            .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id", order)
            .unwrap();
        assert_eq!(ids(&b), want, "{order:?}");
    }
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, v FROM t ORDER BY v, id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), want, "a pipeline");
    // Byte order puts ' a' second and 😀 last; ICU puts the emoji among the symbols.
    assert_ne!(want, (1..=40).collect::<Vec<i64>>());
}

#[test]
fn a_condition_on_text_under_icu_compares_by_the_collation_on_both_paths() {
    let path = snail_of("icu-cond", ICU, &FORTY);
    let mut snail = Snail::open(&path).unwrap();
    let (_, c) = open(&Spec::icu("en-US", None, true), "test").unwrap();
    let below: Vec<i64> = (1..=40)
        .filter(|&i| c.cmp(FORTY[i as usize - 1].as_bytes(), b"b") == Ordering::Less)
        .collect();
    for order in [Some("t_v"), Some("t_pkey")] {
        let (b, _) = snail
            .ask(
                "SELECT t.id FROM s.t t WHERE t.v < 'b' ORDER BY t.id",
                order,
            )
            .unwrap();
        assert_eq!(ids(&b), below, "{order:?}");
        let (b, _) = snail
            .ask(
                "SELECT t.id FROM s.t t WHERE t.v = 'Á' ORDER BY t.id",
                order,
            )
            .unwrap();
        assert_eq!(ids(&b), vec![10], "{order:?}");
    }
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v < 'b' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), below);
    let (b, _) = snail
        .ask("SELECT min(t.v), max(t.v) FROM s.t t", None)
        .unwrap();
    let least = sorted(&Spec::icu("en-US", None, true), &FORTY)[0];
    assert_eq!(texts(&b, 0), vec![FORTY[least as usize - 1].to_string()]);
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT min(v) AS x, max(v) AS y FROM t",
            None,
        )
        .unwrap();
    let order = sorted(&Spec::icu("en-US", None, true), &FORTY);
    let most = order[order.len() - 1];
    assert_eq!(
        (texts(&b, 0), texts(&b, 1)),
        (
            vec![FORTY[least as usize - 1].to_string()],
            vec![FORTY[most as usize - 1].to_string()]
        )
    );
    // A COLLATE on either side compares by it, over the column's own.
    let bytes: Vec<i64> = (1..=40)
        .filter(|&i| FORTY[i as usize - 1].as_bytes() < b"b".as_slice())
        .collect();
    for w in ["v COLLATE \"C\" < 'b'", "v < 'b' COLLATE \"C\""] {
        let (b, _) = snail
            .ask(
                &format!(
                    "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE {w} ORDER BY id"
                ),
                None,
            )
            .unwrap();
        assert_eq!(ids(&b), bytes, "{w}");
    }
}

#[test]
fn a_nondeterministic_collation_keys_equal_values_alike_and_reads_them_as_one() {
    let ddl = "CREATE COLLATION s.ci (provider = icu, locale = 'und-u-ks-level2', \
               deterministic = false); \
               CREATE TABLE s.t (id integer NOT NULL, v text COLLATE s.ci, PRIMARY KEY (id)); \
               CREATE INDEX t_v ON s.t (v, id);";
    let values = ["a", "A", "b", "B", "ab", "AB", "Ab", "c"];
    let path = snail_of("icu-ci", ddl, &values);
    let mut snail = Snail::open(&path).unwrap();
    for order in [Some("t_v"), Some("t_pkey")] {
        let (b, _) = snail
            .ask(
                "SELECT t.id FROM s.t t WHERE t.v = 'a' ORDER BY t.id",
                order,
            )
            .unwrap();
        assert_eq!(ids(&b), vec![1, 2], "{order:?}");
        let (b, _) = snail
            .ask("SELECT count(*) FROM s.t t WHERE t.v = 'AB'", order)
            .unwrap();
        assert_eq!(one(&b), 3, "{order:?}");
    }
    let (b, _) = snail
        .ask(
            "SELECT t.v, count(*) FROM s.t t GROUP BY t.v ORDER BY t.v",
            None,
        )
        .unwrap();
    assert_eq!(
        ints64(b.column(1).as_ref()),
        vec![Some(2), Some(3), Some(2), Some(1)]
    );
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT v, count(*) AS n FROM t GROUP BY v",
            None,
        )
        .unwrap();
    assert_eq!(b.num_rows(), 4);
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v LIKE 'a%' ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(ids(&b), vec![1, 2, 5, 6, 7]);
    let e = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id FROM t WHERE v ILIKE 'a%'",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("not supported for ILIKE"), "{e}");
}

#[test]
fn case_is_mapped_under_the_column_s_collation() {
    let ddl = "CREATE TABLE s.t (id integer NOT NULL, v text COLLATE \"tr-x-icu\", \
               PRIMARY KEY (id));";
    let path = snail_of("icu-case", ddl, &["ijssel", "straße"]);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "WITH t AS (SELECT id, v FROM s.t) SELECT id, upper(v) AS u FROM t ORDER BY id",
            None,
        )
        .unwrap();
    assert_eq!(texts(&b, 1), vec!["İJSSEL", "STRASSE"]);
}

#[test]
fn glibc_s_order_is_refused_where_strxfrm_l_and_strcoll_l_disagree() {
    let ddl = "CREATE TABLE s.t (id integer NOT NULL, v text COLLATE \"en_US\", \
               PRIMARY KEY (id)); CREATE INDEX t_v ON s.t (v, id);";
    let path = snail_of("libc-forty", ddl, &FORTY);
    let want = sorted(&Spec::libc("en_US.utf8", "en_US.utf8"), &FORTY);
    let mut snail = Snail::open(&path).unwrap();
    let (b, _) = snail
        .ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(ids(&b), want);
    let catalog = Catalog::parse(ddl).unwrap();
    let ids = Values::Int(vec![Some(1), Some(2)]);
    let v = Values::Text(vec![Some("1b".into()), Some("1 B".into())]);
    let e = write(
        &tmp("libc-disagree.snail"),
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![ids, v],
            },
        )],
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("strxfrm_l") && e.contains("strcoll_l"), "{e}");
}

#[test]
fn an_order_cut_under_another_version_is_refused_by_name_and_the_others_read() {
    let path = snail_of("icu-version", ICU, &FORTY);
    let (_, c) = open(&Spec::icu("en-US", None, true), "test").unwrap();
    let recorded = format!("version {}", c.version());
    let mut bytes = std::fs::read(&path).unwrap();
    let mut other = recorded.clone().into_bytes();
    let last = other.len() - 1;
    other[last] = if other[last] == b'0' { b'1' } else { b'0' };
    let mut n = 0;
    let mut at = 0;
    while let Some(i) = bytes[at..]
        .windows(recorded.len())
        .position(|w| w == recorded.as_bytes())
    {
        bytes[at + i..at + i + other.len()].copy_from_slice(&other);
        at += i + other.len();
        n += 1;
    }
    assert!(n >= 1, "the order records its version");
    std::fs::write(&path, &bytes).unwrap();
    let mut snail = Snail::open(&path).unwrap();
    let e = snail
        .ask("SELECT t.id FROM s.t t WHERE t.v = 'a'", Some("t_v"))
        .unwrap_err()
        .to_string();
    assert!(e.contains("t_v") && e.contains("re-cut"), "{e}");
    let (b, _) = snail
        .ask("SELECT t.id FROM s.t t WHERE t.id = 3", Some("t_pkey"))
        .unwrap();
    assert_eq!(ids(&b), vec![3]);
    // Re-cut, every order is cut afresh under this build's ICU and read along again.
    let to = tmp("icu-version-recut.snail");
    let _ = std::fs::remove_file(&to);
    snail.recut(&to).unwrap();
    let mut again = Snail::open(&to).unwrap();
    let (b, _) = again
        .ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(ids(&b), sorted(&Spec::icu("en-US", None, true), &FORTY));
    again.census().unwrap();
}

#[test]
fn rows_written_under_a_collation_are_keyed_by_it_and_a_re_cut_carries_the_sequences() {
    let ddl = "CREATE TABLE s.t (id integer GENERATED BY DEFAULT AS IDENTITY, \
               v text COLLATE \"en-US-x-icu\", PRIMARY KEY (id)); \
               CREATE INDEX t_v ON s.t (v, id);";
    let path = tmp("icu-writes.snail");
    write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    for v in ["b", "B", "a", "\u{e1}", "A"] {
        w.execute(&format!("INSERT INTO s.t (v) VALUES ('{v}')"))
            .unwrap();
    }
    w.execute("UPDATE s.t SET v = 'c' WHERE v = 'B'").unwrap();
    w.execute("DELETE FROM s.t WHERE v = 'a'").unwrap();
    let (b, _) = w
        .ask(
            "SELECT t.id, t.v FROM s.t t ORDER BY t.v, t.id",
            Some("t_v"),
        )
        .unwrap();
    assert_eq!(texts(&b, 1), vec!["A", "\u{e1}", "b", "c"]);
    drop(w);
    let to = tmp("icu-writes-recut.snail");
    let _ = std::fs::remove_file(&to);
    Snail::open_writing(&path).unwrap().recut(&to).unwrap();
    let mut w = Snail::open_writing(&to).unwrap();
    w.execute("INSERT INTO s.t (v) VALUES ('d')").unwrap();
    let (b, _) = w
        .ask("SELECT t.id FROM s.t t WHERE t.v = 'd'", Some("t_v"))
        .unwrap();
    assert_eq!(ids(&b), vec![6]);
}
