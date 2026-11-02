//! A BRIN index is no strand: the block is its run of pages, and each block of the table's order,
//! and each entry above the blocks, sums the key as its operator class does: minmax by the least and
//! greatest an entry keeps of every leaf, inclusion by a span, or a box's leaves' least and greatest,
//! minmax-multi by its runs, bloom by a filter of a power of two of bits. Each question here is asked
//! of a table that declares the index and of its twin that does not: the same rows, every read its
//! predicted pages, and blocks ruled out only where they hold no row the condition takes.

mod weather;

use arrow::array::{Array, Int64Array};
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::composed::scalar_in;
use warren_snail::read::statement::Datum;
use warren_snail::read::{Round, Snail};
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{exact, fresh, tmp, write};

/// The ids an answer holds, rising; an answer of no row may carry no type.
fn rows_of(b: &arrow::array::RecordBatch) -> Vec<i64> {
    if b.num_rows() == 0 {
        return Vec::new();
    }
    let a = b.column(0).as_any().downcast_ref::<Int64Array>().unwrap();
    let mut v: Vec<i64> = (0..a.len()).map(|i| a.value(i)).collect();
    v.sort_unstable();
    v
}

/// `s.t (id, v)` of `n` rows, `v` each row's text, with the index statements given.
fn snail_of(
    name: &str,
    ty: &str,
    index: &str,
    n: i64,
    value: &dyn Fn(i64) -> Option<String>,
) -> PathBuf {
    let path = tmp(&format!("brin-{name}.snail"));
    let catalog = Catalog::parse(&format!(
        "CREATE TABLE s.t (id bigint NOT NULL, v {ty}, PRIMARY KEY (id)); {index}"
    ))
    .unwrap_or_else(|e| panic!("{name}: {e}"));
    let table = catalog.table("s.t").unwrap();
    let id = Values::Int((0..n).map(Some).collect());
    let mut v = Values::new(table.columns[1].ty);
    for i in 0..n {
        let d = match value(i) {
            Some(t) => scalar_in(table.columns[1].ty, &t).unwrap_or_else(|e| panic!("{t}: {e}")),
            None => Datum::Null,
        };
        push_datum(&mut v, &d);
    }
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, v],
            },
        )],
    )
    .unwrap();
    path
}

/// A table that declares the index, and its twin that does not.
fn twins(
    name: &str,
    ty: &str,
    index: &str,
    n: i64,
    value: &dyn Fn(i64) -> Option<String>,
) -> (PathBuf, PathBuf) {
    (
        snail_of(name, ty, index, n, value),
        snail_of(&format!("{name}-twin"), ty, "", n, value),
    )
}

fn ids(s: &mut Snail, q: &str) -> (Vec<i64>, Round) {
    let (b, round) = s
        .ask(q, Some("t_pkey"))
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    exact(q, &round);
    (rows_of(&b), round)
}

/// A question on both: the same rows, and the rounds along the table's order.
fn both(with: &mut Snail, twin: &mut Snail, q: &str) -> (Vec<i64>, Round, Round) {
    let (a, ra) = ids(with, q);
    let (b, rb) = ids(twin, q);
    assert_eq!(a, b, "{q}");
    assert!(
        ra.blocks_read <= rb.blocks_read,
        "{q}: {ra:?} against {rb:?}"
    );
    (a, ra, rb)
}

/// Sixteen blocks of 1,024 rows, each holding -1 and 1,000,000,000 and 62 values of its own, all
/// even: a minmax rules none of them out, for any value between.
fn own_values(i: i64) -> Option<String> {
    Some(match i % 64 {
        0 => "-1".to_string(),
        1 => "1000000000".to_string(),
        r => ((i / 1024) * 1000 + r).wrapping_mul(2).to_string(),
    })
}

#[test]
fn a_filter_rules_out_each_block_its_value_is_not_in_and_never_the_one_it_is_in() {
    let (a, b) = twins(
        "bloom",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops);",
        16384,
        &own_values,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    // A value of the fifth block's own.
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 8010");
    assert_eq!(rows.len(), 16);
    assert_eq!(without.blocks_read, 16, "{without:?}");
    assert!(with.blocks_read <= 2, "{with:?}");
    assert!(with.column_pages_read < without.column_pages_read);
    // A value no row holds.
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 8011");
    assert!(rows.is_empty());
    assert!(with.blocks_read <= 1, "{with:?}");
    // Every value of every block is found, along a filter that may keep others.
    for v in (0..16).flat_map(|b| (2..64).map(move |r| (b * 1000 + r) * 2)) {
        let (rows, _, _) = both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE v = {v}"));
        assert_eq!(rows.len(), 16, "{v}");
    }
    // The values both ends of every block hold, read in every block.
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = -1");
    assert_eq!((rows.len(), with.blocks_read), (256, 16));
    // A few values walked, a comparison left to the rows, each side of the operator.
    let (rows, with, _) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v >= 8008 AND v <= 8012",
    );
    assert_eq!(rows.len(), 3 * 16);
    assert!(with.blocks_read <= 3, "{with:?}");
    let (_, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v < 8010");
    assert_eq!(with.blocks_read, 16);
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE 8010 = v");
    assert_eq!(rows.len(), 16);
    assert!(with.blocks_read <= 2, "{with:?}");
}

#[test]
fn the_runs_rule_out_a_value_between_them_that_a_least_and_greatest_would_keep() {
    // Each block's values lie in two clusters, and between them no row.
    let value = |i: i64| {
        let at = (i / 1024) * 16 + i % 8;
        Some(if i % 2 == 0 { at } else { at + 1_000_000 }.to_string())
    };
    let (a, b) = twins(
        "multi",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_minmax_multi_ops (values_per_range = 8));",
        16384,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 500000");
    assert!(rows.is_empty());
    assert_eq!(without.blocks_read, 16, "{without:?}");
    assert_eq!(with.blocks_read, 0, "{with:?}");
    let (rows, with, _) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v > 400000 AND v < 500000",
    );
    assert!(rows.is_empty());
    assert_eq!(with.blocks_read, 0, "{with:?}");
    // A value of one block's second cluster, and one past every cluster but the last.
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 1000161");
    assert!(!rows.is_empty());
    assert!(with.blocks_read <= 2, "{with:?}");
    let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v >= 1000240");
    assert!(!rows.is_empty());
    let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v <> 500000");
    assert_eq!(rows.len(), 16384);
    // A block's few values stay points, a value between them in no run.
    let (a, b) = twins(
        "multi-points",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_minmax_multi_ops (values_per_range = 8));",
        4096,
        &|i: i64| Some(((i % 4 + 1) * 10).to_string()),
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 25");
    assert!(rows.is_empty());
    assert!(without.blocks_read > 1, "{without:?}");
    assert_eq!(with.blocks_read, 0, "{with:?}");
    let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 30");
    assert_eq!(rows.len(), 1024);
    // On a value keyed by its bytes, runs and points alike.
    let (a, b) = twins(
        "multi-inet",
        "inet",
        "CREATE INDEX t_v ON s.t USING brin (v inet_minmax_multi_ops (values_per_range = 8));",
        4096,
        &|i: i64| {
            Some(if i % 2 == 0 {
                format!("10.0.{}.1", i % 8)
            } else {
                format!("192.168.{}.{}", i / 256, i % 256)
            })
        },
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    for (q, some) in [
        ("v = '10.0.2.1'", true),
        ("v = '192.168.1.1'", true),
        ("v = '172.16.0.1'", false),
        ("v > '172.16.0.1'", true),
        ("v > '172.16.0.1' AND v < '172.16.0.9'", false),
    ] {
        let (rows, with, without) = both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
        assert_eq!(!rows.is_empty(), some, "{q}");
        if !some {
            assert!(without.blocks_read > 1, "{q}: {without:?}");
            assert_eq!(with.blocks_read, 0, "{q}: {with:?}");
        }
    }
}

/// A value of each type a BRIN operator class takes, rising with `j`.
fn rising(ty: &str, j: i64) -> String {
    let date = format!(
        "{:04}-{:02}-{:02}",
        2000 + j / 336,
        1 + (j / 28) % 12,
        1 + j % 28
    );
    let clock = format!("{:02}:{:02}:{:02}", (j / 3600) % 24, (j / 60) % 60, j % 60);
    match ty {
        "smallint" | "integer" | "bigint" | "oid" => format!("{j}"),
        "real" | "double precision" => format!("{j}.5"),
        "numeric" => format!("{j}.50"),
        "numeric(10,2)" => format!("{j}.25"),
        "numeric(30,2)" => format!("{j}.75"),
        "date" => date,
        "timestamp" => format!("{date} 12:00:00"),
        "timestamptz" => format!("{date} 12:00:00+00"),
        "time" => clock,
        "timetz" => format!("{clock}+00"),
        "interval" => format!("{j} days"),
        "uuid" => format!("00000000-0000-0000-0000-{j:012x}"),
        "pg_lsn" => format!("0/{j:X}"),
        "macaddr" => format!("00:00:00:00:{:02x}:{:02x}", j / 256, j % 256),
        "macaddr8" => format!("00:00:00:00:00:00:{:02x}:{:02x}", j / 256, j % 256),
        "inet" | "cidr" => format!("10.0.{}.{}", j / 256, j % 256),
        "text" | "varchar" | "varchar(20)" | "char(12)" | "name" => format!("v{j:08}"),
        "bytea" => format!("\\x{j:08x}"),
        "\"char\"" => char::from(33 + (j * 90 / 4096) as u8).to_string(),
        "bit(32)" | "bit varying" => format!("{j:032b}"),
        "tid" => format!("({j},1)"),
        other => panic!("{other}"),
    }
}

/// Each type a BRIN class takes beside its prefix, whether it has a minmax-multi, and a bloom.
const TYPES: [(&str, &str, bool, bool); 29] = [
    ("smallint", "int2", true, true),
    ("integer", "int4", true, true),
    ("bigint", "int8", true, true),
    ("real", "float4", true, true),
    ("double precision", "float8", true, true),
    ("numeric", "numeric", true, true),
    ("numeric(10,2)", "numeric", true, true),
    ("numeric(30,2)", "numeric", true, true),
    ("date", "date", true, true),
    ("timestamp", "timestamp", true, true),
    ("timestamptz", "timestamptz", true, true),
    ("time", "time", true, true),
    ("timetz", "timetz", true, true),
    ("interval", "interval", true, true),
    ("uuid", "uuid", true, true),
    ("oid", "oid", true, true),
    ("pg_lsn", "pg_lsn", true, true),
    ("macaddr", "macaddr", true, true),
    ("macaddr8", "macaddr8", true, true),
    ("inet", "inet", true, true),
    ("text", "text", false, true),
    ("varchar(20)", "text", false, true),
    ("char(12)", "bpchar", false, true),
    ("name", "name", false, true),
    ("bytea", "bytea", false, true),
    ("\"char\"", "char", false, true),
    ("bit(32)", "bit", false, false),
    ("bit varying", "varbit", false, false),
    ("tid", "tid", true, true),
];

/// Every operator class a type takes, by its prefix.
fn classes(prefix: &str, multi: bool, bloom: bool) -> Vec<String> {
    let mut out = vec![format!("{prefix}_minmax_ops")];
    if multi {
        out.push(format!("{prefix}_minmax_multi_ops"));
    }
    if bloom {
        out.push(format!("{prefix}_bloom_ops"));
    }
    out
}

#[test]
fn each_operator_class_rules_a_block_out_by_its_summary_and_answers_as_its_twin() {
    let n = 4096;
    for (k, (ty, prefix, multi, bloom)) in TYPES.iter().enumerate() {
        // Every tenth row NULL.
        let value = |i: i64| (i % 10 != 3).then(|| rising(ty, i));
        let twin = snail_of(&format!("each-{k}-twin"), ty, "", n, &value);
        let mut t = fresh(&twin);
        for (c, class) in classes(prefix, *multi, *bloom).iter().enumerate() {
            let with = snail_of(
                &format!("each-{k}-{c}"),
                ty,
                &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
                n,
                &value,
            );
            let mut s = fresh(&with);
            let at = |j: i64| rising(ty, j).replace('\'', "''");
            // A value a row holds near the middle, one no row holds (a NULL's), and every comparison.
            for (q, holds) in [
                (format!("v = '{}'", at(2050)), Some(true)),
                (format!("v = '{}'", at(2053)), None),
                (format!("v < '{}'", at(40)), Some(true)),
                (format!("v <= '{}'", at(40)), Some(true)),
                (format!("v > '{}'", at(4050)), Some(true)),
                (format!("v >= '{}'", at(4050)), Some(true)),
                (format!("v <> '{}'", at(2050)), Some(true)),
                ("v IS NULL".to_string(), Some(true)),
                ("v IS NOT NULL".to_string(), Some(true)),
            ] {
                let q = format!("SELECT id FROM s.t WHERE {q}");
                let (rows, with, without) = both(&mut s, &mut t, &q);
                if holds == Some(true) && *ty != "\"char\"" {
                    assert!(!rows.is_empty(), "{ty} {class}: {q}");
                }
                // A rising value lies in one block's bounds, and a narrow comparison in few.
                if q.contains("= '") && !q.contains("<") && !q.contains(">") && *ty != "\"char\"" {
                    let blocks = without.entries_none + without.entries_may + without.entries_must;
                    assert!(
                        with.blocks_read <= 3 && blocks >= with.blocks_read,
                        "{ty} {class}: {q}: {with:?}"
                    );
                }
            }
        }
    }
}

#[test]
fn every_brin_operator_class_of_nineteen_is_held() {
    let held: Vec<_> = warren_snail::lists::opclasses(19)
        .into_iter()
        .filter(|p| p.listed.family == "brin")
        .collect();
    assert_eq!(held.len(), 72);
    let refused: Vec<_> = held
        .iter()
        .filter(|p| p.refused.is_some())
        .map(|p| (p.listed.name, p.refused.clone()))
        .collect();
    assert!(refused.is_empty(), "{refused:?}");
}

#[test]
fn a_brin_index_is_read_with_its_parameters_and_several_keys_as_postgresql_19_reads_it() {
    let table = "CREATE TABLE s.t (id int PRIMARY KEY, a int, b int, c text, r int4range);";
    let read = Catalog::read(&format!(
        "{table} CREATE INDEX i ON s.t USING brin (a, b int4_bloom_ops (false_positive_rate = 0.05, \
         n_distinct_per_range = 100), c text_bloom_ops, r range_inclusion_ops) \
         WITH (pages_per_range = 32, autosummarize = on);"
    ))
    .unwrap();
    assert!(read.statements.iter().all(|s| s.refused.is_none()));
    let notices: Vec<&String> = read.statements.iter().flat_map(|s| &s.notices).collect();
    assert!(
        notices.iter().any(|n| n.contains("pages_per_range = 32")
            && n.contains("autosummarize = on")
            && n.contains("does nothing")),
        "{notices:?}"
    );
    let catalog = read.catalog;
    let t = catalog.table("s.t").unwrap();
    // No strand: only the table's order.
    assert_eq!(t.orders.len(), 1);
    let b = &t.brins[0];
    assert_eq!(b.name, "i");
    assert_eq!(
        b.params[1]
            .as_deref()
            .map(|p| p.replace(' ', ""))
            .as_deref(),
        Some("false_positive_rate=0.05,n_distinct_per_range=100")
    );
    assert_eq!(
        b.opclasses,
        [
            "int4_minmax_ops",
            "int4_bloom_ops",
            "text_bloom_ops",
            "range_inclusion_ops"
        ]
    );
    let facts = catalog.facts();
    let fact = |attr: &str| {
        facts
            .iter()
            .find(|f| f.kind == "index" && f.object == "s.t.i" && f.fact == attr)
            .map(|f| f.value.clone())
    };
    assert_eq!(fact("method").as_deref(), Some("brin"));
    assert_eq!(
        fact("options").as_deref(),
        Some("pages_per_range=32,autosummarize=on")
    );
    // Each refusal as PostgreSQL 19 RC1 gave it (28819, 2026-11-02).
    let outcome = |ddl: &str| -> Option<String> {
        Catalog::read(&format!("{table} {ddl}"))
            .unwrap()
            .statements
            .into_iter()
            .find_map(|s| s.refused)
    };
    for (ddl, why) in [
        (
            "CREATE UNIQUE INDEX e ON s.t USING brin (a)",
            "access method \"brin\" does not support unique indexes",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a) INCLUDE (b)",
            "access method \"brin\" does not support included columns",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a DESC)",
            "access method \"brin\" does not support ASC/DESC options",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a NULLS FIRST)",
            "access method \"brin\" does not support NULLS FIRST/LAST options",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a) WITH (pages_per_range = 0)",
            "value 0 out of bounds for option \"pages_per_range\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a) WITH (pages_per_range = 131073)",
            "value 131073 out of bounds for option \"pages_per_range\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a) WITH (autosummarize = maybe)",
            "invalid value for boolean option \"autosummarize\": maybe",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a) WITH (fillfactor = 50)",
            "unrecognized parameter \"fillfactor\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_minmax_multi_ops (values_per_range = 7))",
            "value 7 out of bounds for option \"values_per_range\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_minmax_multi_ops (values_per_range = 257))",
            "value 257 out of bounds for option \"values_per_range\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_bloom_ops (false_positive_rate = 0.00001))",
            "value 0.00001 out of bounds for option \"false_positive_rate\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_bloom_ops (false_positive_rate = 0.26))",
            "value 0.26 out of bounds for option \"false_positive_rate\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_bloom_ops (n_distinct_per_range = -1.5))",
            "value -1.5 out of bounds for option \"n_distinct_per_range\"",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (a int4_minmax_ops (values_per_range = 8))",
            "operator class int4_minmax_ops has no options",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (c int4_bloom_ops)",
            "operator class \"int4_bloom_ops\" does not accept data type text",
        ),
        (
            "CREATE INDEX e ON s.t USING brin (c text_minmax_multi_ops)",
            "operator class \"text_minmax_multi_ops\" does not exist for access method \"brin\"",
        ),
    ] {
        let got = outcome(ddl).unwrap_or_default();
        assert!(got.ends_with(why), "{ddl}: {got}");
    }
    // A BRIN is no order.
    let clustered = Catalog::read(&format!(
        "{table} CREATE INDEX i ON s.t USING brin (a); ALTER TABLE s.t CLUSTER ON i;"
    ))
    .unwrap();
    assert!(clustered.statements.iter().any(|s| s
        .refused
        .as_deref()
        .is_some_and(|r| r.contains("does not support clustering"))));
}

#[test]
fn several_keys_and_several_indexes_on_one_column_are_each_summed_and_read() {
    let catalog = Catalog::parse(
        "CREATE TABLE s.t (id bigint NOT NULL, a integer, b integer, c integer, PRIMARY KEY (id));
         CREATE INDEX t_ab ON s.t USING brin (a int4_bloom_ops, b int4_minmax_multi_ops \
         (values_per_range = 8), c);
         CREATE INDEX t_a2 ON s.t USING brin (a int4_minmax_multi_ops (values_per_range = 8));
         CREATE INDEX t_a3 ON s.t USING brin (a int4_bloom_ops);",
    )
    .unwrap();
    let table = catalog.table("s.t").unwrap();
    // A bloom and runs on a, runs on b; the bloom named twice is kept once, c's minmax is the leaf's.
    assert_eq!(warren_snail::brin::kept_count(table, &table.orders[0]), 3);
    let n = 16384i64;
    let path = tmp("brin-several.snail");
    let ints = |f: &dyn Fn(i64) -> i64| Values::Int((0..n).map(|i| Some(f(i))).collect());
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![
                    ints(&|i| i),
                    ints(&|i| match i % 64 {
                        0 => -1,
                        1 => 1_000_000_000,
                        r => ((i / 1024) * 1000 + r) * 2,
                    }),
                    ints(&|i| {
                        let at = (i / 1024) * 16 + i % 8;
                        if i % 2 == 0 {
                            at
                        } else {
                            at + 1_000_000
                        }
                    }),
                    ints(&|i| i),
                ],
            },
        )],
    )
    .unwrap();
    let mut s = fresh(&path);
    for (q, most) in [
        ("SELECT id FROM s.t WHERE a = 8010", 2),
        ("SELECT id FROM s.t WHERE b = 500000", 0),
        ("SELECT id FROM s.t WHERE c = 5000", 1),
        ("SELECT id FROM s.t WHERE a = 8010 AND b = 500000", 0),
    ] {
        let (_, round) = ids(&mut s, q);
        assert!(round.blocks_read <= most, "{q}: {round:?}");
    }
}

#[test]
fn nan_and_the_infinities_are_summed_as_postgresql_compares_them_and_never_ruled_out() {
    let specials = ["NaN", "Infinity", "-Infinity", "-0", "0", "1.5"];
    let value = |i: i64| {
        // The last block's first rows hold what the others do not, and every sixteenth row NULL.
        if i % 16 == 15 {
            None
        } else if i >= 15360 {
            Some(specials[(i % 6) as usize].to_string())
        } else {
            Some(format!("{}.25", i / 1024 + 10))
        }
    };
    for (k, class) in [
        "float8_minmax_ops",
        "float8_minmax_multi_ops",
        "float8_bloom_ops",
    ]
    .iter()
    .enumerate()
    {
        let (a, b) = twins(
            &format!("nan-{k}"),
            "double precision",
            &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
            16384,
            &value,
        );
        let (mut s, mut t) = (fresh(&a), fresh(&b));
        for (q, some) in [
            ("v = 'NaN'", true),
            ("v = 'Infinity'", true),
            ("v = '-Infinity'", true),
            ("v = 0", true),
            ("v = '-0'", true),
            ("v > 100", true),
            ("v >= 'NaN'", true),
            ("v < 'NaN'", true),
            ("v < 0", true),
            ("v = 20.25", true),
            ("v = 2.5", false),
        ] {
            let (rows, with, _) = both(
                &mut s,
                &mut t,
                &format!("SELECT id FROM s.t WHERE {q} ORDER BY id"),
            );
            assert_eq!(!rows.is_empty(), some, "{class}: {q}");
            // A value the other blocks do not hold rules them out.
            if q == "v = 'NaN'" {
                assert!(with.blocks_read <= 2, "{class}: {with:?}");
            }
        }
    }
    // A real, a numeric of each form: a value and its class's other spellings.
    for (ty, prefix, spellings) in [
        ("real", "float4", ["'0'", "'-0'", "0", "'NaN'"]),
        ("numeric", "numeric", ["1.0", "1.000", "1", "'NaN'"]),
        ("numeric(10,2)", "numeric", ["1.0", "1.000", "1", "'NaN'"]),
    ] {
        let value = |i: i64| {
            Some(match i % 4 {
                0 => if prefix == "float4" { "-0" } else { "1.00" }.to_string(),
                1 => "NaN".to_string(),
                _ => format!("{}", 100 + i),
            })
        };
        for class in [
            format!("{prefix}_minmax_ops"),
            format!("{prefix}_minmax_multi_ops"),
            format!("{prefix}_bloom_ops"),
        ] {
            let (a, b) = twins(
                &format!("class-{}-{class}", ty.replace(['(', ')', ','], "")),
                ty,
                &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
                4096,
                &value,
            );
            let (mut s, mut t) = (fresh(&a), fresh(&b));
            for v in spellings {
                let q = format!("SELECT id FROM s.t WHERE v = {v}");
                let (rows, _, _) = both(&mut s, &mut t, &q);
                assert_eq!(rows.len(), 1024, "{ty} {class}: {q}");
            }
        }
    }
}

#[test]
fn a_block_holding_only_nulls_is_ruled_out_of_a_comparison_and_kept_for_is_null() {
    // The first four blocks' rows all NULL, on a column of each summary.
    let value = |i: i64| (i >= 4096).then(|| ((i % 64) * 2).to_string());
    for (k, class) in ["int4_minmax_ops", "int4_minmax_multi_ops", "int4_bloom_ops"]
        .iter()
        .enumerate()
    {
        let (a, b) = twins(
            &format!("nulls-{k}"),
            "integer",
            &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
            16384,
            &value,
        );
        let (mut s, mut t) = (fresh(&a), fresh(&b));
        let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 10");
        assert_eq!(rows.len(), 192);
        assert_eq!(with.blocks_read, 12, "{class}: {with:?}");
        let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v IS NULL");
        assert_eq!(rows.len(), 4096);
        assert!(with.blocks_read <= 8, "{class}: {with:?}");
        let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v IS NOT NULL");
        assert_eq!(rows.len(), 12288);
    }
}

#[test]
fn each_string_s_class_is_summed_so_a_spelling_of_a_value_finds_it() {
    let class = |ty: &str, prefix: &str| {
        let value = |i: i64| Some(format!("v{:04}", i % 200));
        let (a, b) = twins(
            &format!("str-{prefix}"),
            ty,
            &format!("CREATE INDEX t_v ON s.t USING brin (v {prefix}_bloom_ops);"),
            4096,
            &value,
        );
        let (mut s, mut t) = (fresh(&a), fresh(&b));
        let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 'v0007'");
        assert_eq!(rows.len(), 21, "{ty}");
        let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 'absent'");
        assert!(rows.is_empty(), "{ty}");
        (s, t)
    };
    class("text", "text");
    class("name", "name");
    // A char(n)'s blanks are no part of its class, written or asked.
    let (mut s, mut t) = class("char(12)", "bpchar");
    let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 'v0007    '");
    assert_eq!(rows.len(), 21);
    // Equal intervals of other spellings.
    let value = |i: i64| Some(if i % 2 == 0 { "1 day" } else { "24 hours" }.to_string());
    for class in [
        "interval_bloom_ops",
        "interval_minmax_multi_ops",
        "interval_minmax_ops",
    ] {
        let (a, b) = twins(
            &format!("iv-{class}"),
            "interval",
            &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
            4096,
            &value,
        );
        let (mut s, mut t) = (fresh(&a), fresh(&b));
        for spelled in ["'1 day'", "'24 hours'", "'1440 minutes'"] {
            let (rows, _, _) = both(
                &mut s,
                &mut t,
                &format!("SELECT id FROM s.t WHERE v = {spelled}"),
            );
            assert_eq!(rows.len(), 4096, "{class}: {spelled}");
        }
    }
}

/// `n` ranges, each block near its own, every hundredth empty and every hundred-and-first NULL.
fn range_value(i: i64) -> Option<String> {
    (i % 101 != 0).then(|| {
        if i % 100 == 0 {
            "empty".to_string()
        } else {
            format!("[{},{})", i * 10, i * 10 + 5)
        }
    })
}

#[test]
fn a_range_s_inclusion_rules_out_a_block_whose_span_its_value_misses() {
    let (a, b) = twins(
        "range",
        "int4range",
        "CREATE INDEX t_v ON s.t USING brin (v range_inclusion_ops);",
        16384,
        &range_value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    for q in [
        "v && '[50000,50100)'",
        "v @> '[50000,50003)'",
        "v @> 50002",
        "v <@ '[50000,50100)'",
        "v << '[1000,1010)'",
        "v >> '[160000,160010)'",
        "v &< '[1000,1010)'",
        "v &> '[160000,160010)'",
        "v -|- '[50005,50010)'",
        "v @> 'empty'",
        "v <@ 'empty'",
        "v && 'empty'",
        "v = 'empty'",
        "v = '[50000,50005)'",
        "v < '[50000,50005)'",
        "v >= '[160000,160005)'",
        "'[50000,50100)' && v",
        "'[50000,50100)' @> v",
    ] {
        both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
    }
    // A narrow value lies in the span of few blocks; the twin keeps no span.
    let (rows, with, without) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v && '[50000,50100)'",
    );
    assert_eq!(rows.len(), 9);
    assert!(with.blocks_read <= 2, "{with:?}");
    assert!(without.blocks_read >= 8, "{without:?}");
    // An empty range is contained by everything, and lies in no span: the blocks holding one are read.
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v <@ 'empty'");
    assert!(!rows.is_empty());
    assert!(with.blocks_read > 0);
}

#[test]
fn an_inet_s_inclusion_rules_out_a_block_whose_addresses_its_network_misses() {
    let value = |i: i64| (i % 97 != 0).then(|| format!("10.{}.{}.0/24", i / 256, i % 256));
    for (name, ty) in [("inet", "inet"), ("cidr", "cidr")] {
        let (a, b) = twins(
            name,
            ty,
            "CREATE INDEX t_v ON s.t USING brin (v inet_inclusion_ops);",
            16384,
            &value,
        );
        let (mut s, mut t) = (fresh(&a), fresh(&b));
        for q in [
            "v && '10.40.0.0/16'",
            "v << '10.40.0.0/16'",
            "v <<= '10.40.0.0/16'",
            "v >> '10.40.5.7'",
            "v >>= '10.40.5.0/24'",
            "v = '10.40.5.0/24'",
            "v && '192.168.0.0/16'",
            "v << '::/0'",
        ] {
            both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
        }
        let (rows, with, without) = both(
            &mut s,
            &mut t,
            "SELECT id FROM s.t WHERE v << '10.40.0.0/16'",
        );
        assert_eq!(rows.len(), 253, "{ty}");
        assert!(with.blocks_read <= 2, "{with:?}");
        assert!(without.blocks_read >= 16, "{without:?}");
        // No address of the family, nor of the network, lies in any block.
        let (rows, with, _) = both(
            &mut s,
            &mut t,
            "SELECT id FROM s.t WHERE v && '192.168.0.0/16'",
        );
        assert!(rows.is_empty());
        assert_eq!(with.blocks_read, 0, "{with:?}");
    }
    // Both families at once, the span holding them all.
    let value = |i: i64| {
        Some(if i % 2 == 0 {
            format!("10.0.0.{}", i % 256)
        } else {
            format!("2001:db8::{:x}", i % 4096)
        })
    };
    let (a, b) = twins(
        "families",
        "inet",
        "CREATE INDEX t_v ON s.t USING brin (v);",
        4096,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    for q in [
        "v << '10.0.0.0/8'",
        "v << '2001:db8::/32'",
        "v && '::/0'",
        "v && '0.0.0.0/0'",
    ] {
        let (rows, _, _) = both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
        assert!(!rows.is_empty(), "{q}");
    }
}

#[test]
fn a_box_s_inclusion_rules_out_a_block_by_its_leaves_for_every_operator_it_takes() {
    let value = |i: i64| {
        (i % 97 != 0).then(|| {
            let (x, y) = (i as f64, (i % 512) as f64);
            format!("({},{}),({},{})", x + 2.0, y + 2.0, x, y)
        })
    };
    let (a, b) = twins(
        "box",
        "box",
        "CREATE INDEX t_v ON s.t USING brin (v box_inclusion_ops);",
        8192,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let q1 = "'(5010,600),(5000,500)'";
    for q in [
        format!("v && {q1}"),
        format!("v << {q1}"),
        format!("v >> {q1}"),
        format!("v &< {q1}"),
        format!("v &> {q1}"),
        format!("v <<| {q1}"),
        format!("v |>> {q1}"),
        format!("v &<| {q1}"),
        format!("v |&> {q1}"),
        format!("v @> {q1}"),
        format!("v <@ {q1}"),
        "v ~= '(5002,394),(5000,392)'".to_string(),
        "v @> point '(5001,393)'".to_string(),
        "point '(5001,393)' <@ v".to_string(),
        format!("{q1} && v"),
        format!("{q1} @> v"),
        format!("{q1} >> v"),
        format!("{q1} <<| v"),
        format!("{q1} |>> v"),
        "v && box '(10,10),(0,0)'".to_string(),
        "v && '(Infinity,Infinity),(-Infinity,-Infinity)'::box".to_string(),
    ] {
        both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
    }
    let (rows, with, without) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v && '(5010,600),(5000,500)'",
    );
    assert!(rows.is_empty() || rows.iter().all(|&i| (4990..=5020).contains(&i)));
    assert!(with.blocks_read <= 2, "{with:?}");
    assert!(with.blocks_read <= without.blocks_read);
    // A box past every box.
    let (rows, with, _) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v @> point '(5001,393)'",
    );
    assert!(!rows.is_empty());
    assert!(with.blocks_read <= 2, "{with:?}");
    let (rows, with, _) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE '(10,10),(0,0)' |>> v",
    );
    assert!(rows.is_empty() || rows.iter().all(|&i| i % 512 < 8));
    assert!(with.blocks_read <= without.blocks_read);
    let (rows, with, _) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v && '(1000,100000),(999,99999)'",
    );
    assert!(rows.is_empty());
    assert_eq!(with.blocks_read, 0, "{with:?}");
}

#[test]
fn a_filter_folded_above_the_blocks_rules_out_a_page_of_entries_unread() {
    // Few values a block, against a filter sized for its rows: an entry above folds them unfilled.
    let value = |i: i64| Some(((i / 1024) * 16 + i % 4).wrapping_mul(2).to_string());
    let n = 131072;
    let (a, b) = twins(
        "fold",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops (false_positive_rate = 0.01));",
        n,
        &|i| {
            value(i).map(|v| {
                if i % 64 == 0 {
                    "-1".to_string()
                } else if i % 64 == 1 {
                    "1000000000".to_string()
                } else {
                    v
                }
            })
        },
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    // A value no block holds: the blocks' entries lie in several pages, and fewer of them are read.
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 3");
    assert!(rows.is_empty());
    assert_eq!(without.blocks_read, 128);
    assert!(with.blocks_read <= 2, "{with:?}");
    assert!(
        with.directory_pages < without.directory_pages,
        "{with:?} against {without:?}"
    );
    // A value a block holds is found under the folded filters above it.
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 4004");
    assert!(!rows.is_empty());
    assert!(with.blocks_read <= 3, "{with:?}");
    // The filters' bytes lie in the directory, counted: read whole, its pages are its predicted.
    let (_, whole, whole_twin) = both(&mut s, &mut t, "SELECT id FROM s.t");
    assert!(whole.directory_pages > whole_twin.directory_pages);
}

#[test]
fn an_entry_past_a_page_lies_in_the_disk_pages_it_takes_and_reads_its_predicted_pages() {
    // 1,024 values a block at one false positive in 10,000: 32,768 bits, 4,096 bytes an entry.
    let (a, b) = twins(
        "wide",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops (n_distinct_per_range = -1, \
         false_positive_rate = 0.0001));",
        16384,
        &|i: i64| Some((i * 2).to_string()),
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 8000");
    assert_eq!(rows, [4000]);
    assert_eq!(with.blocks_read, 1, "{with:?}");
    assert!(with.directory_pages > without.directory_pages);
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 8001");
    assert!(rows.is_empty());
    assert_eq!(with.blocks_read, 0, "{with:?}");
}

#[test]
fn a_summary_of_a_month_or_a_scaled_key_reads_the_conditions_on_it() {
    let clock = "CREATE FUNCTION s.clock(timestamptz) RETURNS timestamp \
                 LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;";
    let path = tmp("brin-month.snail");
    let catalog = Catalog::parse(&format!(
        "{clock} CREATE TABLE s.t (id bigint NOT NULL, at timestamptz, f double precision, \
         PRIMARY KEY (id)); \
         CREATE INDEX t_m ON s.t USING brin ((extract(month FROM s.clock(at))::smallint)); \
         CREATE INDEX t_b ON s.t USING brin (\
           (extract(month FROM s.clock(at))::smallint) int2_bloom_ops); \
         CREATE INDEX t_f ON s.t USING brin (floor(f * 1000) float8_minmax_multi_ops);"
    ))
    .unwrap();
    let table = catalog.table("s.t").unwrap();
    assert_eq!(warren_snail::brin::kept_count(table, &table.orders[0]), 3);
    let n = 16384i64;
    let text = |i: i64| format!("2020-{:02}-15 12:00:00+00", 1 + (i / 1024) % 12);
    let mut at = Values::new(table.columns[1].ty);
    let mut f = Values::new(table.columns[2].ty);
    for i in 0..n {
        push_datum(&mut at, &scalar_in(table.columns[1].ty, &text(i)).unwrap());
        push_datum(
            &mut f,
            &scalar_in(table.columns[2].ty, &format!("{}.25", i / 1024)).unwrap(),
        );
    }
    let id = Values::Int((0..n).map(Some).collect());
    write(
        &path,
        &catalog,
        &[(
            "s.t".into(),
            Rows {
                columns: vec![id, at, f],
            },
        )],
    )
    .unwrap();
    let mut s = fresh(&path);
    let month = "extract(month FROM s.clock(at))::smallint";
    // Sixteen blocks of 1,024, a month each, its month's rows alone read.
    let (rows, with) = ids(&mut s, &format!("SELECT id FROM s.t WHERE {month} = 3"));
    assert_eq!(rows.len(), 2048);
    assert!(with.blocks_read <= 2 + 2, "{with:?}");
    let (rows, with) = ids(&mut s, "SELECT id FROM s.t WHERE floor(f * 1000) = 5250");
    assert_eq!(rows.len(), 1024);
    assert!(with.blocks_read <= 2, "{with:?}");
}

#[test]
fn a_summary_read_keeps_to_the_round_s_room() {
    // Filters of 4,096 bytes an entry: read and let go at each entry, none kept past it.
    let (a, b) = twins(
        "room",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops (n_distinct_per_range = -1, \
         false_positive_rate = 0.0001));",
        16384,
        &|i: i64| Some((i * 2).to_string()),
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let sizes = || warren_snail::room::Sizes::of(1 << 20);
    s.size_rounds(sizes());
    t.size_rounds(sizes());
    for q in [
        "SELECT id FROM s.t WHERE v >= 0",
        "SELECT id FROM s.t WHERE v = 8000",
    ] {
        let (_, with, _) = both(&mut s, &mut t, q);
        assert!(with.bytes_held <= 1 << 20, "{q}: {with:?}");
    }
}

#[test]
fn a_partial_brin_sums_every_row_and_answers_any_question() {
    let value = |i: i64| Some(((i / 1024) * 1000 + i % 64).to_string());
    let (a, b) = twins(
        "partial",
        "integer",
        "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops) WHERE v > 100;",
        16384,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let (rows, _, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 5");
    assert_eq!(rows.len(), 16);
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 8010");
    assert_eq!(rows.len(), 16);
    assert!(with.blocks_read <= 2, "{with:?}");
    assert_eq!(
        Catalog::parse(
            "CREATE TABLE s.t (id bigint NOT NULL, v integer, PRIMARY KEY (id)); \
             CREATE INDEX t_v ON s.t USING brin (v) WHERE v > 100;"
        )
        .unwrap()
        .table("s.t")
        .unwrap()
        .brins[0]
            .predicate
            .as_deref(),
        Some("v > 100")
    );
}

/// The file's pages past its root's two.
fn pages_past_the_root(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap()[2 * 4096..].to_vec()
}

fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, &b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

#[test]
fn a_table_declaring_no_brin_keeps_its_pages_byte_for_byte() {
    let ints = |n: i64, f: &dyn Fn(i64) -> i64| Values::Int((0..n).map(|i| Some(f(i))).collect());
    let rows = |n: i64| Rows {
        columns: vec![ints(n, &|i| i), ints(n, &|i| (i * 7919) % 1000)],
    };
    let written = |name: &str, ddl: &str, tables: &[(String, Rows)]| {
        let path = tmp(&format!("brin-bytes-{name}.snail"));
        warren_snail::write::write_spanned(
            &path,
            &Catalog::parse(ddl).unwrap(),
            tables,
            &[],
            warren_snail::file::FILE_PAGES,
        )
        .unwrap();
        path
    };
    const PLAIN: &str = "CREATE TABLE s.u (id bigint NOT NULL, v integer, PRIMARY KEY (id));";
    let alone = written("alone", PLAIN, &[("s.u".into(), rows(16384))]);
    // The bytes a table declaring none has always written (crate e99804f, `cmp` of the two builds' files
    // past their roots), by their sum: no field of a summary lies in its head, entries or maps.
    assert_eq!(fnv(&pages_past_the_root(&alone)), 0x63c9_a205_cae8_1b80);
    // Beside a table that declares one, its directory and its pages are the same bytes.
    let beside = written(
        "beside",
        &format!(
            "{PLAIN} CREATE TABLE s.z (id bigint NOT NULL, v integer, PRIMARY KEY (id)); \
             CREATE INDEX z_v ON s.z USING brin (v int4_bloom_ops (n_distinct_per_range = -1));"
        ),
        &[("s.u".into(), rows(16384)), ("s.z".into(), rows(16384))],
    );
    let plain = written(
        "beside-plain",
        &format!("{PLAIN} CREATE TABLE s.z (id bigint NOT NULL, v integer, PRIMARY KEY (id));"),
        &[("s.u".into(), rows(16384)), ("s.z".into(), rows(16384))],
    );
    let top = |path: &Path, table: &str| -> (Vec<warren_snail::read::BlockRow>, u64) {
        let s = Snail::open(path).unwrap();
        let rows = s
            .blocks()
            .unwrap()
            .into_iter()
            .filter(|b| b.table == table)
            .collect::<Vec<_>>();
        let pages = rows.iter().map(|b| b.disk_pages).sum();
        (rows, pages)
    };
    assert_eq!(top(&beside, "s.u").1, top(&plain, "s.u").1);
    assert_eq!(top(&beside, "s.z").1, top(&plain, "s.z").1);
    assert_eq!(top(&beside, "s.u").0.len(), top(&plain, "s.u").0.len());
    let (mut a, mut b) = (fresh(&beside), fresh(&plain));
    // The table declaring one reads more of its directory, the other the same.
    let q = "SELECT id FROM s.u";
    assert_eq!(
        a.ask(q, None).unwrap().1.directory_pages,
        b.ask(q, None).unwrap().1.directory_pages
    );
    let q = "SELECT id FROM s.z";
    assert!(a.ask(q, None).unwrap().1.directory_pages > b.ask(q, None).unwrap().1.directory_pages);
}

#[test]
fn a_snail_of_format_eight_refuses_a_brin_by_name_with_the_way_across() {
    let e = warren_snail::write::write_format_eight(
        &tmp("brin-eight.snail"),
        &Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b integer, PRIMARY KEY (a)); \
             CREATE INDEX t_b ON s.t USING brin (b);",
        )
        .unwrap(),
        &[],
        weather::span(),
    )
    .err()
    .unwrap()
    .to_string();
    assert!(
        e.contains("s.t: the brin index t_b, which a snail of format 8") && e.contains("`out`"),
        "{e}"
    );
}

#[test]
fn a_filter_on_text_rules_out_each_block_its_value_is_not_in() {
    // Each block's own values, and both ends of every block's: its bounds rule out none between them.
    let value = |i: i64| {
        Some(match i % 64 {
            0 => "a".to_string(),
            1 => "z".to_string(),
            r => format!("m{:04}{:02}", i / 1024, r),
        })
    };
    let (a, b) = twins(
        "text-bloom",
        "text",
        "CREATE INDEX t_v ON s.t USING brin (v text_bloom_ops);",
        16384,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    let (rows, with, without) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 'm000710'");
    assert_eq!(rows.len(), 16);
    assert!(without.blocks_read >= 16, "{without:?}");
    assert!(with.blocks_read <= 4, "{with:?}");
    let (rows, with, _) = both(&mut s, &mut t, "SELECT id FROM s.t WHERE v = 'm000799'");
    assert!(rows.is_empty());
    assert!(with.blocks_read <= 1, "{with:?}");
    // A value the filter does not test: every block, by what its rows hold.
    let (rows, with, whole) = both(
        &mut s,
        &mut t,
        "SELECT id FROM s.t WHERE v > 'm0007' AND v < 'm0008'",
    );
    assert_eq!(rows.len(), 16 * 62);
    assert_eq!(with.blocks_read, whole.blocks_read, "{with:?}");
}

#[test]
fn a_numeric_past_a_double_precision_s_range_is_taken_as_any_other_value() {
    // PostgreSQL refuses these rows in its minmax-multi, whose distance is a double precision
    // (`brin_minmax_multi_distance_numeric`): what a summary keeps does not stand in a row's way.
    let value = |i: i64| {
        Some(match i % 512 {
            0 => "1e131071".to_string(),
            1 => "-1e131071".to_string(),
            r => format!("{r}"),
        })
    };
    let (a, b) = twins(
        "numeric-far",
        "numeric",
        "CREATE INDEX t_v ON s.t USING brin (v numeric_minmax_multi_ops);",
        4096,
        &value,
    );
    let (mut s, mut t) = (fresh(&a), fresh(&b));
    for (q, n) in [
        ("v = 1e131071", 8),
        ("v = -1e131071", 8),
        ("v = 7", 8),
        ("v = 7000", 0),
    ] {
        let (rows, _, _) = both(&mut s, &mut t, &format!("SELECT id FROM s.t WHERE {q}"));
        assert_eq!(rows.len(), n, "{q}");
    }
}

#[cfg(feature = "collations")]
#[test]
fn a_summary_under_a_library_s_collation_sums_each_value_s_key() {
    for (name, collate, value, asked) in [
        ("icu", "COLLATE \"en-US-x-icu\"", "abc", "abc"),
        (
            "ci",
            "COLLATE s.ci",
            "abc",
            // Equal under the collation, so one key.
            "ABC",
        ),
    ] {
        let declared = "CREATE COLLATION s.ci (provider = icu, locale = 'und-u-ks-level2', \
                        deterministic = false);";
        let path = tmp(&format!("brin-collated-{name}.snail"));
        let catalog = Catalog::parse(&format!(
            "{declared} CREATE TABLE s.t (id bigint NOT NULL, v text {collate}, PRIMARY KEY (id)); \
             CREATE INDEX t_v ON s.t USING brin (v text_bloom_ops);"
        ))
        .unwrap();
        let keys = Values::Int((0..4096).map(Some).collect());
        let v = Values::Text(
            (0..4096)
                .map(|i| {
                    Some(if i == 2000 {
                        value.to_string()
                    } else {
                        format!("q{}", i % 64)
                    })
                })
                .collect(),
        );
        write(
            &path,
            &catalog,
            &[(
                "s.t".into(),
                Rows {
                    columns: vec![keys, v],
                },
            )],
        )
        .unwrap();
        let mut s = fresh(&path);
        let (rows, round) = ids(&mut s, &format!("SELECT id FROM s.t WHERE v = '{asked}'"));
        assert_eq!(rows, [2000], "{name}");
        assert!(round.blocks_read <= 2, "{name}: {round:?}");
    }
}

#[cfg(feature = "standalone")]
mod written {
    use super::*;

    fn run(w: &mut Snail, sql: &str) -> warren_snail::read::Done {
        w.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"))
    }

    const COLUMNS: &str = "id bigint NOT NULL, b integer, m integer, x text, r int4range, n inet, \
                           q box, PRIMARY KEY (id)";
    const INDEXES: &str = "CREATE INDEX t_b ON s.t USING brin (b int4_bloom_ops);
         CREATE INDEX t_m ON s.t USING brin (m int4_minmax_multi_ops (values_per_range = 8));
         CREATE INDEX t_x ON s.t USING brin (x text_bloom_ops);
         CREATE INDEX t_r ON s.t USING brin (r);
         CREATE INDEX t_n ON s.t USING brin (n);
         CREATE INDEX t_q ON s.t USING brin (q);";

    fn row(i: i64) -> String {
        format!(
            "({i}, {}, {}, 'x{:04}', '[{},{})', '10.{}.{}.1', '({},{}),({},{})')",
            (i / 1024) * 1000 + i % 64,
            if i % 2 == 0 {
                i / 64
            } else {
                i / 64 + 1_000_000
            },
            (i / 1024) * 10 + i % 8,
            i * 10,
            i * 10 + 5,
            i / 256,
            i % 256,
            i + 1,
            i + 1,
            i,
            i
        )
    }

    /// A snail of the table, its first rows inserted, with or without the indexes.
    fn table(name: &str, index: &str, n: i64) -> PathBuf {
        let path = tmp(&format!("brin-written-{name}.snail"));
        write(
            &path,
            &Catalog::parse(&format!("CREATE TABLE s.t ({COLUMNS}); {index}")).unwrap(),
            &[],
        )
        .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        for at in (0..n).step_by(512) {
            let rows: Vec<String> = (at..(at + 512).min(n)).map(row).collect();
            run(
                &mut w,
                &format!("INSERT INTO s.t VALUES {}", rows.join(", ")),
            );
        }
        path
    }

    const QUESTIONS: [&str; 15] = [
        "b = 8010",
        "b = 77777",
        "b = 555",
        "m = 40",
        "m = 500000",
        "m = 1000040",
        "m < 3",
        "x = 'x0085'",
        "x = 'absent'",
        "r && '[40000,40020)'",
        "r @> 40002",
        "n << '10.15.0.0/16'",
        "n && '192.168.0.0/16'",
        "q && '(4002,4002),(4000,4000)'",
        "q << '(10,10),(0,0)'",
    ];

    /// Every question asked of both, on writers' own rounds.
    fn asked(a: &mut Snail, b: &mut Snail, also: &[&str]) {
        for q in QUESTIONS.iter().chain(also) {
            let q = format!("SELECT id FROM s.t WHERE {q}");
            let (x, _) = a.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
            let (y, _) = b.ask(&q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
            assert_eq!(rows_of(&x), rows_of(&y), "{q}");
        }
    }

    #[test]
    fn a_summary_is_kept_right_through_the_write_the_update_the_delete_the_cut_and_the_prune() {
        let (a, b) = (table("kept", INDEXES, 8192), table("kept-twin", "", 8192));
        let (mut s, mut t) = (
            Snail::open_writing(&a).unwrap(),
            Snail::open_writing(&b).unwrap(),
        );
        asked(&mut s, &mut t, &[]);
        let both = |s: &mut Snail, t: &mut Snail, sql: &str| {
            run(s, sql);
            run(t, sql);
        };
        // A row into a block's tail, whose value none held: its summary widens.
        both(
            &mut s,
            &mut t,
            "INSERT INTO s.t VALUES (100000, 77777, 500000, 'absent', '[900000,900005)', \
             '192.168.1.1', '(1000000,1000000),(999999,999999)')",
        );
        asked(
            &mut s,
            &mut t,
            &["r @> 900002", "q && '(1000000,1000000),(999999,999999)'"],
        );
        // An update moving a row's values, in its block; a delete; a transaction's own rows.
        both(&mut s, &mut t, "UPDATE s.t SET b = 66666, m = 700000, x = 'moved', r = '[800000,800005)', n = '172.16.0.1' WHERE id = 5");
        asked(
            &mut s,
            &mut t,
            &[
                "b = 66666",
                "m = 700000",
                "x = 'moved'",
                "r @> 800002",
                "n << '172.16.0.0/12'",
            ],
        );
        both(&mut s, &mut t, "DELETE FROM s.t WHERE id = 6");
        asked(&mut s, &mut t, &[]);
        run(&mut s, "BEGIN");
        run(&mut t, "BEGIN");
        both(&mut s, &mut t, "INSERT INTO s.t VALUES (100001, 55555, 600000, 'mine', '[700000,700005)', '172.17.0.1', '(5,5),(4,4)')");
        asked(
            &mut s,
            &mut t,
            &[
                "b = 55555",
                "m = 600000",
                "x = 'mine'",
                "r @> 700002",
                "n << '172.17.0.0/16'",
            ],
        );
        run(&mut s, "ROLLBACK");
        run(&mut t, "ROLLBACK");
        asked(&mut s, &mut t, &["b = 55555", "m = 600000", "x = 'mine'"]);
        // A key updated out of its block, and rows enough to cut a block.
        both(&mut s, &mut t, "UPDATE s.t SET id = 200000 WHERE id = 7");
        asked(&mut s, &mut t, &[]);
        let rows: Vec<String> = (8192..16384).map(row).collect();
        for chunk in rows.chunks(256) {
            both(
                &mut s,
                &mut t,
                &format!("INSERT INTO s.t VALUES {}", chunk.join(", ")),
            );
        }
        asked(&mut s, &mut t, &["b = 12010", "m = 200", "x = 'x0100'"]);
        s.census().unwrap();
    }

    #[test]
    fn a_prune_writes_its_block_s_summary_afresh_from_the_rows_it_keeps() {
        for (k, class) in ["int4_bloom_ops", "int4_minmax_multi_ops"]
            .iter()
            .enumerate()
        {
            let path = snail_of(
                &format!("prune-{k}"),
                "integer",
                &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
                16384,
                &own_values,
            );
            let mut w = Snail::open_writing(&path).unwrap();
            // A row into the last block's tail, whose value none held; deleted, its value stays summed.
            run(&mut w, "INSERT INTO s.t VALUES (16384, 666666)");
            run(&mut w, "DELETE FROM s.t WHERE id = 16384");
            let q = "SELECT id FROM s.t WHERE v = 666666";
            // Read whole, the block is read a page for that row alone, and pruned.
            let (b, round) = w.ask(q, None).unwrap();
            exact(q, &round);
            assert_eq!(
                (b.num_rows(), round.blocks_read),
                (0, 1),
                "{class}: {round:?}"
            );
            assert_eq!(round.blocks_pruned, 1, "{class}: {round:?}");
            // Its summary is written afresh from the rows it keeps: the block is ruled out.
            let (b, round) = w.ask(q, None).unwrap();
            exact(q, &round);
            assert_eq!(
                (b.num_rows(), round.blocks_read),
                (0, 0),
                "{class}: {round:?}"
            );
        }
    }

    #[test]
    fn a_block_holding_only_nulls_sums_its_first_value_written() {
        for (k, class) in ["int4_bloom_ops", "int4_minmax_multi_ops"]
            .iter()
            .enumerate()
        {
            let path = snail_of(
                &format!("first-{k}"),
                "integer",
                &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
                4096,
                &|_| None,
            );
            let mut w = Snail::open_writing(&path).unwrap();
            let q = "SELECT id FROM s.t WHERE v = 5";
            let (b, round) = w.ask(q, None).unwrap();
            assert_eq!(
                (b.num_rows(), round.blocks_read),
                (0, 0),
                "{class}: {round:?}"
            );
            run(&mut w, "UPDATE s.t SET v = 5 WHERE id = 100");
            let (b, round) = w.ask(q, None).unwrap();
            exact(q, &round);
            assert_eq!(
                (b.num_rows(), round.blocks_read),
                (1, 1),
                "{class}: {round:?}"
            );
        }
    }

    #[test]
    fn a_writer_of_one_snapshot_writes_a_summary_right_in_the_view_block() {
        for level in ["REPEATABLE READ", "SERIALIZABLE"] {
            let path = table(&format!("view-{}", &level[..3]), INDEXES, 4096);
            let (mut a, mut c) = (
                Snail::open_writing(&path).unwrap(),
                Snail::open_writing(&path).unwrap(),
            );
            run(&mut a, &format!("BEGIN ISOLATION LEVEL {level}"));
            run(&mut c, &format!("BEGIN ISOLATION LEVEL {level}"));
            // Two rows of one block, each its own new values: each finds its own and not the other's.
            run(
                &mut a,
                "UPDATE s.t SET b = 91002, m = 910020, x = 'a-up' WHERE id = 101",
            );
            run(
                &mut c,
                "UPDATE s.t SET b = 92002, m = 920020, x = 'c-up' WHERE id = 102",
            );
            let count = |w: &mut Snail, q: &str| {
                w.ask(&format!("SELECT id FROM s.t WHERE {q}"), None)
                    .unwrap()
                    .0
                    .num_rows()
            };
            assert_eq!(count(&mut a, "b = 91002"), 1, "{level}");
            assert_eq!(count(&mut a, "m = 910020"), 1, "{level}");
            assert_eq!(count(&mut a, "x = 'a-up'"), 1, "{level}");
            assert_eq!(count(&mut a, "b = 92002"), 0, "{level}");
            assert_eq!(count(&mut c, "x = 'c-up'"), 1, "{level}");
            assert_eq!(count(&mut c, "b = 91002"), 0, "{level}");
            run(&mut a, "COMMIT");
            match c.execute("COMMIT") {
                Ok(_) => {}
                // SERIALIZABLE's scans mark the block: the second to commit may be refused.
                Err(e) if level == "SERIALIZABLE" => {
                    assert!(e.to_string().contains("serialize"), "{e}")
                }
                Err(e) => panic!("{e}"),
            }
            let mut r = fresh(&path);
            let ids = |r: &mut Snail, q: &str| {
                let (b, round) = r
                    .ask(&format!("SELECT id FROM s.t WHERE {q}"), None)
                    .unwrap();
                exact(q, &round);
                b.num_rows()
            };
            assert_eq!(ids(&mut r, "b = 91002"), 1, "{level}");
            assert_eq!(ids(&mut r, "m = 910020"), 1, "{level}");
            assert_eq!(ids(&mut r, "x = 'a-up'"), 1, "{level}");
            if level == "REPEATABLE READ" {
                assert_eq!(ids(&mut r, "b = 92002"), 1, "{level}");
                assert_eq!(ids(&mut r, "m = 920020"), 1, "{level}");
                assert_eq!(ids(&mut r, "x = 'c-up'"), 1, "{level}");
            }
        }
    }

    #[test]
    fn a_brin_declared_on_an_empty_table_is_summed_and_on_one_holding_rows_is_refused() {
        let path = tmp("brin-declared.snail");
        write(
            &path,
            &Catalog::parse("CREATE TABLE s.t (id bigint NOT NULL, v integer, PRIMARY KEY (id));")
                .unwrap(),
            &[],
        )
        .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        run(
            &mut w,
            "CREATE INDEX t_v ON s.t USING brin (v int4_bloom_ops)",
        );
        let rows: Vec<String> = (0..16384)
            .map(|i| format!("({i}, {})", (i / 1024) * 1000 + i % 64))
            .collect();
        for chunk in rows.chunks(1024) {
            run(
                &mut w,
                &format!("INSERT INTO s.t VALUES {}", chunk.join(", ")),
            );
        }
        let (b, round) = w.ask("SELECT id FROM s.t WHERE v = 8010", None).unwrap();
        exact("v = 8010", &round);
        assert_eq!(b.num_rows(), 16);
        assert!(round.blocks_read <= 2, "{round:?}");
        let e = w
            .execute("CREATE INDEX t_w ON s.t USING brin (v)")
            .err()
            .unwrap()
            .to_string();
        assert!(e.contains("it holds rows"), "{e}");
    }

    #[test]
    fn a_row_added_to_a_block_is_summed_for_every_type_a_class_takes() {
        for (k, (ty, prefix, multi, bloom)) in TYPES.iter().enumerate() {
            for (c, class) in classes(prefix, *multi, *bloom).iter().enumerate() {
                let path = snail_of(
                    &format!("added-{k}-{c}"),
                    ty,
                    &format!("CREATE INDEX t_v ON s.t USING brin (v {class});"),
                    2048,
                    &|i: i64| Some(rising(ty, i)),
                );
                let mut w = Snail::open_writing(&path).unwrap();
                // A value past every value the table holds, in the last block's tail, and one set on a
                // row of its first; each found, and the rows it was not given are not.
                let (past, again) = (rising(ty, 3000), rising(ty, 3001));
                let literal = |v: &str| v.replace('\'', "''");
                run(
                    &mut w,
                    &format!("INSERT INTO s.t VALUES (5000, '{}')", literal(&past)),
                );
                run(
                    &mut w,
                    &format!("UPDATE s.t SET v = '{}' WHERE id = 1", literal(&again)),
                );
                for v in [&past, &again] {
                    let q = format!("SELECT id FROM s.t WHERE v = '{}'", literal(v));
                    let (b, round) = w
                        .ask(&q, None)
                        .unwrap_or_else(|e| panic!("{ty} {class}: {e}"));
                    assert!(b.num_rows() >= 1, "{ty} {class}: {q}");
                    assert!(round.blocks_read >= 1, "{ty} {class}: {q}: {round:?}");
                }
            }
        }
    }

    #[test]
    fn a_table_another_declared_a_brin_of_since_refuses_its_rows() {
        let path = tmp("brin-declared-since.snail");
        write(
            &path,
            &Catalog::parse(
                "CREATE TABLE s.t (id bigint NOT NULL, v integer, PRIMARY KEY (id)); \
                 CREATE TABLE s.e (id bigint NOT NULL, v integer, PRIMARY KEY (id));",
            )
            .unwrap(),
            &[],
        )
        .unwrap();
        let (mut a, mut b) = (
            Snail::open_writing(&path).unwrap(),
            Snail::open_writing(&path).unwrap(),
        );
        run(&mut a, "BEGIN ISOLATION LEVEL REPEATABLE READ");
        assert_eq!(a.ask("SELECT id FROM s.t", None).unwrap().0.num_rows(), 0);
        run(
            &mut b,
            "CREATE INDEX e_v ON s.e USING brin (v int4_bloom_ops)",
        );
        run(&mut a, "INSERT INTO s.e (id, v) VALUES (1, 1)");
        assert_eq!(
            a.execute("COMMIT").err().unwrap().to_string(),
            "could not serialize access due to concurrent update"
        );
    }

    #[test]
    fn a_brin_table_goes_out_and_comes_back_with_its_summaries() {
        let path = table("out", INDEXES, 2048);
        let dir = tmp("brin-out-dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        let outs = w.out(&dir).unwrap();
        let catalog = Catalog::parse(&format!("CREATE TABLE s.t ({COLUMNS}); {INDEXES}")).unwrap();
        let back = tmp("brin-back.snail");
        warren_snail::write::write_parquet(
            &back,
            &catalog,
            &[("s.t".to_string(), outs[0].file.clone())],
            warren_snail::file::FILE_PAGES,
            None,
            None,
        )
        .unwrap();
        let mut r = fresh(&back);
        let (mut s, _) = (Snail::open_writing(&path).unwrap(), ());
        asked(
            &mut r,
            &mut s,
            &["b = 1010", "m = 10", "m = 1000010", "x = 'x0012'"],
        );
        for q in ["b = 1010", "m = 10", "m = 1000010", "x = 'x0012'"] {
            let q = format!("SELECT id FROM s.t WHERE {q}");
            assert!(r.ask(&q, None).unwrap().0.num_rows() > 0, "{q}");
        }
        let (_, round) = r.ask("SELECT id FROM s.t WHERE m = 500000", None).unwrap();
        assert_eq!(round.blocks_read, 0, "{round:?}");
    }
}
