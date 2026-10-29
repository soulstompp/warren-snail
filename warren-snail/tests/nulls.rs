//! A level's NULL rows on their own cycle: a run after every value of the level, or before them under
//! NULLS FIRST, cut on the levels after it and counted on its own. No comparison matches a NULL, IS
//! NULL is counted from the counts, a unique index takes any number of NULLs unless they are not
//! distinct, and formats 7 and 8 refuse a NULL on a level by name.

mod weather;

use arrow::array::RecordBatch;
use warren_snail::catalog::Catalog;
use warren_snail::read::{BlockRow, Round, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;
use weather::{bag, exact, ints64, one, tmp, write};

/// 8,192 rows: `a` the key; `b` NULL on every fourth row, else a value from -500 to 499; `s` NULL on
/// every fifth row, else one of 37 words, the empty one among them.
const N: i64 = 8192;

fn b_of(a: i64) -> Option<i64> {
    (a % 4 != 0).then_some(a % 1000 - 500)
}

fn s_of(a: i64) -> Option<String> {
    (a % 5 != 0).then(|| match a % 37 {
        3 => String::new(),
        w => format!("w{w:02}"),
    })
}

const DDL: &str = "CREATE TABLE s.t (a bigint NOT NULL, b integer, s text, PRIMARY KEY (a));
CREATE INDEX t_b ON s.t (b, a);
CREATE INDEX t_bf ON s.t (b NULLS FIRST, a);
CREATE INDEX t_s ON s.t (s, a);
CREATE INDEX t_bs ON s.t (b, s);";

fn rows(n: i64) -> Rows {
    Rows {
        columns: vec![
            Values::Int((0..n).map(Some).collect()),
            Values::Int((0..n).map(b_of).collect()),
            Values::Text((0..n).map(s_of).collect()),
        ],
    }
}

fn written(name: &str, ddl: &str, n: i64) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &Catalog::parse(ddl).unwrap(),
        &[("s.t".to_string(), rows(n))],
    )
    .unwrap();
    path
}

fn ask(s: &mut Snail, sql: &str, order: Option<&str>) -> (RecordBatch, Round) {
    let (b, round) = s
        .ask(sql, order)
        .unwrap_or_else(|e| panic!("{sql} along {order:?}: {e}"));
    exact(sql, &round);
    (b, round)
}

/// The blocks of an order, once each, in its order.
fn blocks_of<'a>(rows: &'a [BlockRow], order: &str) -> Vec<&'a BlockRow> {
    rows.iter()
        .filter(|r| r.order == order && r.column == "a")
        .collect()
}

#[test]
fn a_level_s_null_rows_lie_on_a_run_of_their_own_after_its_values_cut_on_the_levels_after_it() {
    let s = Snail::open(&written("null-runs", DDL, N)).unwrap();
    let rows = s.blocks().unwrap();
    let nulls = (0..N).filter(|&a| b_of(a).is_none()).count() as u64;
    for (order, first) in [("t_b", false), ("t_bf", true)] {
        let blocks = blocks_of(&rows, order);
        let is_null = |r: &BlockRow| r.first.starts_with("[NULL");
        // The NULL rows' blocks: together at the end, or at the start under NULLS FIRST.
        let at: Vec<bool> = blocks.iter().map(|r| is_null(r)).collect();
        let run = at.iter().filter(|&&n| n).count();
        let want: Vec<bool> = if first {
            (0..at.len()).map(|i| i < run).collect()
        } else {
            (0..at.len()).map(|i| i >= at.len() - run).collect()
        };
        assert_eq!(at, want, "{order}");
        // They hold the NULL rows alone, every one of them, and none holds a value of b.
        let null_blocks: Vec<&&BlockRow> = blocks.iter().filter(|r| is_null(r)).collect();
        assert!(
            null_blocks.iter().all(|r| r.last.starts_with("[NULL")),
            "{order}"
        );
        assert_eq!(
            null_blocks.iter().map(|r| r.rows).sum::<u64>(),
            nulls,
            "{order}"
        );
        // Cut on the levels after it: a's pages of 512 rows each fill, so the run is four blocks,
        // their first keys rising on a.
        assert_eq!(null_blocks.len(), 4, "{order}");
        let firsts: Vec<i64> = null_blocks
            .iter()
            .map(|r| {
                r.first
                    .trim_end_matches(']')
                    .rsplit(", ")
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap()
            })
            .collect();
        assert!(
            firsts.windows(2).all(|w| w[0] < w[1]),
            "{order}: {firsts:?}"
        );
        // b's NULLs are counted in each block, every row of the run.
        let b_nulls: u64 = rows
            .iter()
            .filter(|r| r.order == order && r.column == "b" && r.first.starts_with("[NULL"))
            .map(|r| r.nulls)
            .sum();
        assert_eq!(b_nulls, nulls, "{order}");
    }
}

#[test]
fn no_comparison_matches_a_null() {
    let mut s = Snail::open(&written("null-compared", DDL, N)).unwrap();
    let count = |f: &dyn Fn(i64) -> bool| (0..N).filter(|&a| f(a)).count() as i64;
    // A bound left open below is the least integer: a NULL counted there would be kept.
    let (on_b, on_s) = (["t_pkey", "t_b", "t_bf"], ["t_pkey", "t_s"]);
    let questions: Vec<(&str, i64, &[&str])> = vec![
        ("b <= 499", count(&|a| b_of(a).is_some()), &on_b),
        ("b >= -500", count(&|a| b_of(a).is_some()), &on_b),
        ("b = -500", count(&|a| b_of(a) == Some(-500)), &on_b),
        ("b < 0", count(&|a| b_of(a).is_some_and(|b| b < 0)), &on_b),
        (
            "b > 400",
            count(&|a| b_of(a).is_some_and(|b| b > 400)),
            &on_b,
        ),
        ("s = ''", count(&|a| s_of(a).as_deref() == Some("")), &on_s),
        ("s >= ''", count(&|a| s_of(a).is_some()), &on_s),
        ("s <= 'w99'", count(&|a| s_of(a).is_some()), &on_s),
        (
            "s > 'w30'",
            count(&|a| s_of(a).is_some_and(|s| s.as_str() > "w30")),
            &on_s,
        ),
        // A comparison with NULL itself keeps no row, as PostgreSQL's does.
        ("b = NULL", 0, &on_b),
        ("NULL <= b", 0, &on_b),
        ("s > NULL", 0, &on_s),
        ("a < NULL", 0, &on_b),
    ];
    for (cond, want, orders) in &questions {
        for &order in *orders {
            let sql = format!("SELECT count(*) FROM s.t WHERE {cond}");
            assert_eq!(
                one(&ask(&mut s, &sql, Some(order)).0),
                *want,
                "{sql} along {order}"
            );
            let sql = format!("SELECT a FROM s.t WHERE {cond}");
            assert_eq!(
                ask(&mut s, &sql, Some(order)).0.num_rows() as i64,
                *want,
                "{sql} along {order}"
            );
        }
    }
}

#[test]
fn an_equality_on_the_empty_text_matches_no_null_in_a_block_holding_both() {
    // 100 rows are one block, holding the empty text first and the NULLs last on t_s.
    let mut s = Snail::open(&written("null-empty", DDL, 100)).unwrap();
    let want = (0..100).filter(|&a| s_of(a).as_deref() == Some("")).count() as i64;
    assert!(want > 0);
    let (b, round) = ask(&mut s, "SELECT a FROM s.t WHERE s = ''", Some("t_s"));
    assert_eq!((b.num_rows() as i64, round.blocks_read), (want, 1));
}

#[test]
fn is_null_is_counted_from_the_counts_and_reads_no_column_page() {
    let mut s = Snail::open(&written("null-counted", DDL, N)).unwrap();
    let count = |f: &dyn Fn(i64) -> bool| (0..N).filter(|&a| f(a)).count() as i64;
    let (b, t) = (["t_pkey", "t_b", "t_bf"], ["t_pkey", "t_s"]);
    let all = ["t_pkey", "t_b", "t_bf", "t_s"];
    for (cond, want, orders) in [
        ("b IS NULL", count(&|a| b_of(a).is_none()), &b[..]),
        ("b IS NOT NULL", count(&|a| b_of(a).is_some()), &b),
        ("s IS NULL", count(&|a| s_of(a).is_none()), &t),
        ("s IS NOT NULL", count(&|a| s_of(a).is_some()), &t),
        ("a IS NULL", 0, &all),
        ("a IS NOT NULL", N, &all),
        ("b IS NULL AND b IS NOT NULL", 0, &b),
        ("b IS NULL AND b = 3", 0, &b),
        ("s IS NULL AND s = ''", 0, &t),
    ] {
        for &order in orders {
            let sql = format!("SELECT count(*) FROM s.t WHERE {cond}");
            let (got, round) = ask(&mut s, &sql, Some(order));
            assert_eq!(one(&got), want, "{sql} along {order}");
            assert_eq!(round.column_pages_read, 0, "{sql} along {order}");
        }
    }
    // In a block holding a level's values beside its NULLs, the NULLs counted close it too.
    let mut one_block = Snail::open(&written("null-counted-one", DDL, 100)).unwrap();
    for (cond, want, orders) in [
        ("b IS NULL", 25, ["t_b", "t_bf", "t_bs"]),
        ("s IS NULL", 20, ["t_s", "t_bs", "t_pkey"]),
    ] {
        for order in orders {
            let sql = format!("SELECT count(*) FROM s.t WHERE {cond}");
            let (got, round) = ask(&mut one_block, &sql, Some(order));
            assert_eq!(
                (one(&got), round.blocks_read, round.column_pages_read),
                (want, 0, 0),
                "{sql} along {order}"
            );
        }
    }
    // Two tests for NULL on two columns close where the counts hold only one of them open.
    let sql = "SELECT count(*) FROM s.t WHERE b IS NULL AND s IS NULL";
    let (got, _) = ask(&mut s, sql, Some("t_pkey"));
    assert_eq!(
        one(&got),
        count(&|a| b_of(a).is_none() && s_of(a).is_none())
    );
}

#[test]
fn is_null_on_a_level_reads_only_its_null_run_and_the_levels_after_it_narrow_it() {
    let mut s = Snail::open(&written("null-narrowed", DDL, N)).unwrap();
    let rows = s.blocks().unwrap();
    let runs = blocks_of(&rows, "t_b")
        .iter()
        .filter(|r| r.first.starts_with("[NULL"))
        .count() as u64;
    let mut want: Vec<Vec<Option<String>>> = (0..N)
        .filter(|&a| b_of(a).is_none())
        .map(|a| vec![Some(a.to_string())])
        .collect();
    want.sort();
    for order in ["t_b", "t_bf"] {
        let (b, round) = ask(&mut s, "SELECT a FROM s.t WHERE b IS NULL", Some(order));
        assert_eq!(bag(&b), want, "{order}");
        assert_eq!(round.blocks_read, runs, "{order}");
        // a fixed after b's NULLs: one block of the run.
        let (b, round) = ask(
            &mut s,
            "SELECT a FROM s.t WHERE b IS NULL AND a >= 3000 AND a < 3100",
            Some(order),
        );
        let want: Vec<i64> = (3000..3100).filter(|&a| b_of(a).is_none()).collect();
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        assert_eq!((got, round.blocks_read), (want, 1), "{order}");
        // IS NOT NULL passes over the run's blocks, whose counts hold no value.
        let all = blocks_of(&rows, order).len() as u64;
        let (_, round) = ask(&mut s, "SELECT a FROM s.t WHERE b IS NOT NULL", Some(order));
        assert_eq!(round.blocks_read, all - runs, "{order}");
    }
}

#[test]
fn is_null_on_a_level_lets_the_levels_after_it_narrow_the_blocks_read() {
    // On t_bs the NULL run of b is cut on s. Fixed on b's NULLs, an equality on s reads the run's
    // blocks that hold it; left a condition to read, it reads every block of the run.
    let mut s = Snail::open(&written("null-then-text", DDL, 4 * N)).unwrap();
    let rows = s.blocks().unwrap();
    let runs = blocks_of(&rows, "t_bs")
        .iter()
        .filter(|r| r.first.starts_with("[NULL"))
        .count() as u64;
    let (b, round) = ask(
        &mut s,
        "SELECT a FROM s.t WHERE b IS NULL AND s = 'w05'",
        Some("t_bs"),
    );
    let want = (0..4 * N)
        .filter(|&a| b_of(a).is_none() && s_of(a).as_deref() == Some("w05"))
        .count();
    assert_eq!(b.num_rows(), want);
    assert!(
        runs >= 4 && round.blocks_read * 2 <= runs,
        "{} of the run's {runs}",
        round.blocks_read
    );
}

#[test]
fn an_answer_is_ordered_with_its_nulls_where_order_by_puts_them() {
    let mut s = Snail::open(&written("null-ordered", DDL, N)).unwrap();
    let a: Vec<i64> = (0..40).collect();
    let key = |x: &i64| b_of(*x);
    for (by, nulls_first, desc) in [
        ("b, a", false, false),
        ("b NULLS FIRST, a", true, false),
        ("b DESC, a", true, true),
        ("b DESC NULLS LAST, a", false, true),
    ] {
        let mut want = a.clone();
        want.sort_by(|x, y| {
            let o = match (key(x), key(y)) {
                (None, None) => std::cmp::Ordering::Equal,
                (None, _) if nulls_first => std::cmp::Ordering::Less,
                (None, _) => std::cmp::Ordering::Greater,
                (_, None) if nulls_first => std::cmp::Ordering::Greater,
                (_, None) => std::cmp::Ordering::Less,
                (Some(p), Some(q)) if desc => q.cmp(&p),
                (Some(p), Some(q)) => p.cmp(&q),
            };
            o.then(x.cmp(y))
        });
        for order in ["t_pkey", "t_b", "t_bf"] {
            let sql = format!("SELECT a, b FROM s.t WHERE a < 40 ORDER BY {by}");
            let (b, _) = ask(&mut s, &sql, Some(order));
            let got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
            assert_eq!(got, want, "{sql} along {order}");
        }
    }
}

const TABLE: &str = "CREATE TABLE s.t (a bigint NOT NULL, b integer, s text, PRIMARY KEY (a));";

#[test]
fn a_unique_index_takes_any_number_of_nulls_unless_they_are_not_distinct() {
    let table = TABLE;
    // b is NULL on 2,048 rows and repeats its values: unique on b and a together, it takes them.
    written(
        "null-unique",
        &format!("{table} CREATE UNIQUE INDEX t_u ON s.t (b, a);"),
        N,
    );
    let e = write(
        &tmp("null-not-distinct.snail"),
        &Catalog::parse(&format!(
            "{table} CREATE UNIQUE INDEX t_u ON s.t (b, s) NULLS NOT DISTINCT;"
        ))
        .unwrap(),
        &[("s.t".to_string(), rows(N))],
    )
    .err()
    .unwrap()
    .to_string();
    // Where b repeats and s is NULL, or both are NULL, the keys are one.
    assert!(
        e.contains("two rows share the value [") && e.contains("NULL] of t_u, which is unique"),
        "{e}"
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_statement_puts_a_null_beside_a_null_under_a_unique_index_unless_they_are_not_distinct() {
    let table = TABLE;
    let distinct = written(
        "null-unique-rows",
        &format!("{table} CREATE UNIQUE INDEX t_u ON s.t (b, a);"),
        N,
    );
    let mut w = Snail::open_writing(&distinct).unwrap();
    w.execute(&format!(
        "INSERT INTO s.t (a, b, s) VALUES ({N}, NULL, 'x')"
    ))
    .unwrap();
    let path = tmp("null-not-distinct-rows.snail");
    write(
        &path,
        &Catalog::parse(&format!(
            "{table} CREATE UNIQUE INDEX t_u ON s.t (b) NULLS NOT DISTINCT;"
        ))
        .unwrap(),
        &[(
            "s.t".to_string(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(0), Some(1)]),
                    Values::Int(vec![None, Some(1)]),
                    Values::Text(vec![None, None]),
                ],
            },
        )],
    )
    .unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    let e = w
        .execute("INSERT INTO s.t (a, b, s) VALUES (2, NULL, NULL)")
        .err()
        .unwrap()
        .to_string();
    assert!(e.contains("two rows share the value [NULL] of t_u"), "{e}");
    w.execute("INSERT INTO s.t (a, b, s) VALUES (2, 2, NULL)")
        .unwrap();
}

#[cfg(feature = "standalone")]
#[test]
fn a_null_on_a_level_is_inserted_updated_and_deleted_as_any_value_is() {
    let path = written("null-written", DDL, N);
    let mut w = Snail::open_writing(&path).unwrap();
    let mut model: Vec<(i64, Option<i64>)> = (0..N).map(|a| (a, b_of(a))).collect();
    w.execute(&format!(
        "INSERT INTO s.t (a, b, s) VALUES ({N}, NULL, NULL), ({}, 7, NULL)",
        N + 1
    ))
    .unwrap();
    model.extend([(N, None), (N + 1, Some(7))]);
    w.execute("UPDATE s.t SET b = NULL WHERE a = 5").unwrap();
    w.execute("UPDATE s.t SET b = 9 WHERE a = 8").unwrap();
    for m in model.iter_mut() {
        if m.0 == 5 {
            m.1 = None;
        }
        if m.0 == 8 {
            m.1 = Some(9);
        }
    }
    w.execute("DELETE FROM s.t WHERE b IS NULL AND a < 100")
        .unwrap();
    model.retain(|m| !(m.1.is_none() && m.0 < 100));
    drop(w);
    let mut s = Snail::open(&path).unwrap();
    for order in ["t_pkey", "t_b", "t_bf"] {
        let nulls = model.iter().filter(|m| m.1.is_none()).count() as i64;
        let (b, _) = ask(
            &mut s,
            "SELECT count(*) FROM s.t WHERE b IS NULL",
            Some(order),
        );
        assert_eq!(one(&b), nulls, "{order}");
        let (b, _) = ask(&mut s, "SELECT a FROM s.t WHERE b IS NULL", Some(order));
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        let want: Vec<i64> = model
            .iter()
            .filter(|m| m.1.is_none())
            .map(|m| m.0)
            .collect();
        assert_eq!(got, want, "{order}");
        let (b, _) = ask(&mut s, "SELECT a FROM s.t WHERE b = 9", Some(order));
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        let want: Vec<i64> = model
            .iter()
            .filter(|m| m.1 == Some(9))
            .map(|m| m.0)
            .collect();
        assert_eq!(got, want, "{order}");
    }
}

#[test]
fn a_join_kept_to_keys_finds_every_row_beside_a_level_s_nulls() {
    // c's pid is NULL on every eightieth row: on c_p its NULLs are too few to fill a page, so they
    // join the last block of values, and the keys at the end are found among them.
    let ddl = "CREATE TABLE s.p (id bigint NOT NULL, PRIMARY KEY (id));
CREATE TABLE s.c (id bigint NOT NULL, pid bigint, PRIMARY KEY (id));
CREATE INDEX c_p ON s.c (pid, id);";
    let pid = |id: i64| (id % 80 != 0).then_some(id / 4);
    let path = tmp("null-joined.snail");
    write(
        &path,
        &Catalog::parse(ddl).unwrap(),
        &[
            (
                "s.p".to_string(),
                Rows {
                    columns: vec![Values::Int((0..N / 4).map(Some).collect())],
                },
            ),
            (
                "s.c".to_string(),
                Rows {
                    columns: vec![
                        Values::Int((0..N).map(Some).collect()),
                        Values::Int((0..N).map(pid).collect()),
                    ],
                },
            ),
        ],
    )
    .unwrap();
    let mut s = Snail::open(&path).unwrap();
    let rows = s.blocks().unwrap();
    let last = rows
        .iter()
        .rev()
        .find(|r| r.order == "c_p" && r.column == "id")
        .unwrap();
    assert!(
        !last.first.starts_with("[NULL") && last.last.starts_with("[NULL"),
        "{last:?}"
    );
    // In that block, IS NULL keeps the NULLs and passes over the values before them: counted, and
    // fixing pid's run ahead of a range on id, where each row's pid is held against the NULL.
    for (q, from) in [
        ("SELECT id FROM s.c WHERE pid IS NULL", 0),
        ("SELECT id FROM s.c WHERE pid IS NULL AND id >= 7000", 7000),
    ] {
        let nulls: Vec<i64> = (from..N).filter(|&id| pid(id).is_none()).collect();
        let (b, _) = ask(&mut s, q, Some("c_p"));
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        assert_eq!(got, nulls, "{q}");
    }
    for from in [N / 4 - 8, N / 4 - 1, 0, 1000] {
        let sql = format!(
            "SELECT c.id FROM s.p p JOIN s.c c ON c.pid = p.id WHERE p.id >= {from} AND p.id < {}",
            from + 8
        );
        let (b, _) = s.ask(&sql, None).unwrap();
        let mut got: Vec<i64> = ints64(b.column(0).as_ref()).into_iter().flatten().collect();
        got.sort();
        let want: Vec<i64> = (0..N)
            .filter(|&id| pid(id).is_some_and(|p| p >= from && p < from + 8))
            .collect();
        assert_eq!(got, want, "{sql}");
    }
}

#[test]
fn formats_seven_and_eight_refuse_a_null_on_a_level_by_name() {
    let catalog = Catalog::parse(DDL).unwrap();
    for name in ["seven", "eight"] {
        let path = tmp(&format!("null-format-{name}.snail"));
        let make = |rows: Rows| {
            let tables = [("s.t".to_string(), rows)];
            if name == "seven" {
                warren_snail::write::write_format_seven(&path, &catalog, &tables, weather::span())
            } else {
                warren_snail::write::write_format_eight(&path, &catalog, &tables, weather::span())
            }
        };
        let e = make(rows(64)).err().unwrap().to_string();
        assert!(
            e.contains("s.t: a level of t_b holds NULL, which a snail of format")
                && e.contains("`out`"),
            "{e}"
        );
        // A nullable level holding no NULL is written, and a statement putting a NULL on it is
        // refused.
        let full = Rows {
            columns: vec![
                Values::Int((0..64).map(Some).collect()),
                Values::Int((0..64).map(Some).collect()),
                Values::Text((0..64).map(|a| Some(a.to_string())).collect()),
            ],
        };
        make(full).unwrap();
        #[cfg(feature = "standalone")]
        {
            let mut w = Snail::open_writing(&path).unwrap();
            let e = w
                .execute("INSERT INTO s.t (a, b, s) VALUES (64, NULL, 'x')")
                .err()
                .unwrap()
                .to_string();
            assert!(
                e.contains("a level of t_b holds NULL, which a snail of format"),
                "{e}"
            );
            w.execute("INSERT INTO s.t (a, b, s) VALUES (64, 64, 'x')")
                .unwrap();
        }
    }
}

/// A table ordered on a unique index of a nullable key, `k`: 2, NULL, 1 and 3.
fn window_snail(name: &str, index: &str) -> std::path::PathBuf {
    let path = tmp(&format!("{name}.snail"));
    write(
        &path,
        &Catalog::parse(&format!(
            "CREATE TABLE s.w (k bigint, v bigint NOT NULL);
             CREATE UNIQUE INDEX w_k ON s.w {index};
             ALTER TABLE s.w CLUSTER ON w_k;"
        ))
        .unwrap(),
        &[(
            "s.w".to_string(),
            Rows {
                columns: vec![
                    Values::Int(vec![Some(2), None, Some(1), Some(3)]),
                    Values::Int(vec![Some(20), Some(40), Some(10), Some(30)]),
                ],
            },
        )],
    )
    .unwrap();
    path
}

/// A key and the rows a window counts from the first to it.
type Counted = (Option<i64>, Option<i64>);

/// Each row's key and its count of the rows from the first to it, as a window counts them.
fn counted_along(s: &mut Snail, nulls: &str) -> Result<Vec<Counted>, String> {
    let sql = format!("SELECT k, count(*) OVER (ORDER BY k{nulls}) AS n FROM s.w");
    let (b, _) = s.ask(&sql, None).map_err(|e| e.to_string())?;
    Ok(ints64(b.column(0).as_ref())
        .into_iter()
        .zip(ints64(b.column(1).as_ref()))
        .collect())
}

#[test]
fn a_window_is_read_along_an_order_only_where_every_row_is_its_own_peer() {
    // A window folds its rows one at a time along the table's order, each its own peer. Two NULL
    // keys are peers where the order takes them as distinct, and NULLs first are not where the
    // window's ascending order puts them.
    let taken = |s: &mut Snail| counted_along(s, "");
    let mut s = Snail::open(&window_snail("null-window", "(k) NULLS NOT DISTINCT")).unwrap();
    assert_eq!(
        taken(&mut s).unwrap(),
        [
            (Some(1), Some(1)),
            (Some(2), Some(2)),
            (Some(3), Some(3)),
            (None, Some(4))
        ]
    );
    let e = counted_along(&mut s, " NULLS FIRST").err().unwrap();
    assert!(e.contains("a window is read only along"), "{e}");
    for index in ["(k)", "(k NULLS FIRST) NULLS NOT DISTINCT"] {
        let e = taken(&mut Snail::open(&window_snail("null-window-refused", index)).unwrap())
            .err()
            .unwrap();
        assert!(e.contains("a window is read only along"), "{index}: {e}");
    }
}

#[cfg(feature = "standalone")]
#[test]
fn a_window_reads_a_block_s_tail_in_the_order_its_nulls_last() {
    // A row inserted goes after the block's last, past the NULL; the window reads it in its place.
    let path = window_snail("null-window-tail", "(k) NULLS NOT DISTINCT");
    Snail::open_writing(&path)
        .unwrap()
        .execute("INSERT INTO s.w (k, v) VALUES (0, 5)")
        .unwrap();
    assert_eq!(
        counted_along(&mut Snail::open(&path).unwrap(), "").unwrap(),
        [
            (Some(0), Some(1)),
            (Some(1), Some(2)),
            (Some(2), Some(3)),
            (Some(3), Some(4)),
            (None, Some(5))
        ]
    );
}

#[cfg(feature = "standalone")]
#[test]
fn a_statement_that_waited_finds_its_rows_again_by_a_key_that_holds_no_null() {
    // Two rows share a NULL key on the table's unique order, which tells them apart no more. The
    // second writer's statement finds the row of id 1 alone; the row of id 2 meets its condition
    // only once the first writer commits, after the statement began, and is not its.
    use std::sync::mpsc;
    use std::time::Duration;
    let path = tmp("null-again.snail");
    write(
        &path,
        &Catalog::parse(
            "CREATE TABLE s.k (k bigint, v bigint NOT NULL, id bigint NOT NULL);
             CREATE UNIQUE INDEX k_k ON s.k (k);
             ALTER TABLE s.k CLUSTER ON k_k;",
        )
        .unwrap(),
        &[(
            "s.k".to_string(),
            Rows {
                columns: vec![
                    Values::Int(vec![None, None, Some(1)]),
                    Values::Int(vec![Some(0), Some(5), Some(7)]),
                    Values::Int(vec![Some(1), Some(2), Some(3)]),
                ],
            },
        )],
    )
    .unwrap();
    let mut one_ = Snail::open_writing(&path).unwrap();
    one_.execute("BEGIN").unwrap();
    one_.execute("UPDATE s.k SET v = 0 WHERE id = 2").unwrap();
    let (tx, rx) = mpsc::channel();
    let p = path.clone();
    std::thread::spawn(move || {
        let mut w = Snail::open_writing(&p).unwrap();
        let done = w.execute("UPDATE s.k SET v = 100 WHERE v = 0");
        tx.send(done.map(|d| d.rows).map_err(|e| e.to_string()))
            .unwrap();
    });
    std::thread::sleep(Duration::from_millis(400));
    assert!(rx.try_recv().is_err(), "the writer did not wait");
    one_.execute("COMMIT").unwrap();
    assert_eq!(rx.recv_timeout(Duration::from_secs(10)).unwrap(), Ok(1));
    let (b, _) = Snail::open(&path)
        .unwrap()
        .ask("SELECT id, v FROM s.k ORDER BY id", None)
        .unwrap();
    assert_eq!(ints64(b.column(1).as_ref()), [Some(100), Some(0), Some(7)]);
}

#[test]
fn a_level_s_null_run_is_no_place_of_it_for_the_counting_unit() {
    // On t_b1, b alone: its values are each a row's, and its NULLs lie beside them, 6,144 rows. Read
    // as one place of b, the run would make every column's unit twelve pages; beside b's places,
    // each of its rows is its own place, and the unit is one page.
    let path = tmp("null-unit.snail");
    write(
        &path,
        &Catalog::parse(
            "CREATE TABLE s.t (a bigint NOT NULL, b integer, PRIMARY KEY (a));
             CREATE INDEX t_b1 ON s.t (b);",
        )
        .unwrap(),
        &[(
            "s.t".to_string(),
            Rows {
                columns: vec![
                    Values::Int((0..N).map(Some).collect()),
                    Values::Int((0..N).map(|a| (a % 4 == 0).then_some(a)).collect()),
                ],
            },
        )],
    )
    .unwrap();
    let rows = Snail::open(&path).unwrap().blocks().unwrap();
    let units: Vec<u64> = rows
        .iter()
        .filter(|r| r.order == "t_b1")
        .map(|r| r.k)
        .collect();
    assert!(
        !units.is_empty() && units.iter().all(|&k| k == 1),
        "{units:?}"
    );
}
