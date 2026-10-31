//! Money read and printed under the lc_monetary its file records: every money value a statement
//! reads or makes read by `cash_in` and written by `cash_out` under the locale's conventions, its
//! casts by the locale's fraction digits, and to_char's currency symbol the locale's. C and POSIX
//! are read by every build; another locale through glibc with the feature `collations`, and refused
//! by name without it.

mod weather;

use arrow::array::{Array, RecordBatch, StringArray};
use std::path::PathBuf;
use warren_snail::catalog::Catalog;
use warren_snail::read::Snail;
use warren_snail::values::Values;
use warren_snail::write::{write_carried, Carried, Rows};
use weather::{span, tmp};

const DDL: &str = "CREATE TABLE s.t (id integer NOT NULL, v money, PRIMARY KEY (id)); \
                   CREATE INDEX t_v ON s.t (v, id);";

/// Each row's money, as its count of the smallest unit.
const COUNTS: [Option<i64>; 5] = [Some(123456789), Some(-150), Some(0), None, Some(i64::MIN)];

fn rows() -> Rows {
    Rows {
        columns: vec![
            Values::Int((1..=COUNTS.len() as i64).map(Some).collect()),
            Values::Int(COUNTS.to_vec()),
        ],
    }
}

fn carried(monetary: &[(&str, &str, &str)]) -> Carried {
    let mut c = Carried::default();
    c.monetary = monetary
        .iter()
        .map(|(t, c, l)| (t.to_string(), c.to_string(), l.to_string()))
        .collect();
    c
}

/// A snail of `DDL`'s table holding `COUNTS`, its money recorded as written under `locale`.
fn snail_under(name: &str, locale: &str) -> warren_snail::Result<PathBuf> {
    let path = tmp(&format!("{name}.snail"));
    write_carried(
        &path,
        &Catalog::parse(DDL).unwrap(),
        &[("s.t".into(), rows())],
        &[],
        span(),
        warren_snail::regs::LATEST,
        &carried(&[("s.t", "v", locale)]),
    )
    .map(|_| path)
}

/// An answer's column as the CLI's `--text` writes it: a money value by its field's lc_monetary.
fn shown(b: &RecordBatch, c: usize) -> Vec<Option<String>> {
    let a = b.column(c);
    if let Some(s) = a.as_any().downcast_ref::<StringArray>() {
        return s.iter().map(|v| v.map(String::from)).collect();
    }
    let ty = warren_snail::scalars::field_scalar(b.schema().field(c)).unwrap();
    (0..a.len())
        .map(|r| {
            (!a.is_null(r)).then(|| {
                let k = warren_snail::scalars::scalar_key_at(ty, a.as_ref(), r).unwrap();
                warren_snail::scalars::scalar_text(ty, k)
            })
        })
        .collect()
}

fn some(v: &[&str]) -> Vec<Option<String>> {
    v.iter()
        .map(|s| (!s.is_empty()).then(|| s.to_string()))
        .collect()
}

#[test]
fn money_under_c_and_posix_reads_and_prints_as_dollars_and_cents_in_every_build() {
    for locale in ["C", "POSIX"] {
        let path = snail_under(&format!("money-{locale}"), locale).unwrap();
        let mut s = Snail::open(&path).unwrap();
        assert_eq!(s.monetary()[0].2, locale);
        let (b, _) = s
            .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.id", None)
            .unwrap();
        assert_eq!(
            shown(&b, 1),
            some(&[
                "$1,234,567.89",
                "-$1.50",
                "$0.00",
                "",
                "-$92,233,720,368,547,758.08"
            ])
        );
        assert!(!b.schema().field(1).metadata().contains_key("lc_monetary"));
    }
}

#[test]
#[cfg(not(feature = "collations"))]
fn money_under_another_locale_is_refused_by_name_without_the_feature() {
    let e = snail_under("money-gb-unbuilt", "en_GB.utf8")
        .unwrap_err()
        .to_string();
    assert!(
        e.contains("s.t.v is money") && e.contains("feature collations"),
        "{e}"
    );
}

#[test]
#[cfg(feature = "standalone")]
fn a_table_made_on_a_snail_holding_no_money_records_its_money_under_c() {
    let path = tmp("money-none.snail");
    weather::write(
        &path,
        &Catalog::parse("CREATE TABLE s.n (id integer NOT NULL, PRIMARY KEY (id));").unwrap(),
        &[],
    )
    .unwrap();
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute("CREATE TABLE s.u (id integer NOT NULL, m money, a money[], PRIMARY KEY (id))")
        .unwrap();
    drop(w);
    let s = Snail::open(&path).unwrap();
    assert_eq!(
        s.monetary()
            .iter()
            .map(|(t, c, l)| format!("{t}.{c} {l}"))
            .collect::<Vec<_>>(),
        ["s.u.m C", "s.u.a C"]
    );
}

#[cfg(feature = "collations")]
mod under_a_locale {
    use super::*;

    fn ids(b: &RecordBatch) -> Vec<i64> {
        weather::ints64(b.column(0).as_ref())
            .into_iter()
            .flatten()
            .collect()
    }

    #[test]
    fn money_under_a_locale_prints_as_postgresql_prints_it_under_that_lc_monetary() {
        // Each as PostgreSQL 18.6 printed it on 28818 under the same lc_monetary (2026-10-31).
        for (locale, want) in [
            (
                "en_GB.utf8",
                [
                    "£1,234,567.89",
                    "-£1.50",
                    "£0.00",
                    "",
                    "-£92,233,720,368,547,758.08",
                ],
            ),
            (
                "en_IL.utf8",
                [
                    "₪ 1,234,567.89",
                    "₪ 1.50-",
                    "₪ 0.00",
                    "",
                    "₪ 92,233,720,368,547,758.08-",
                ],
            ),
            (
                "en_PH.utf8",
                [
                    "₱1,234,567.89",
                    "(₱1.50)",
                    "₱0.00",
                    "",
                    "(₱92,233,720,368,547,758.08)",
                ],
            ),
            (
                "en_DK.utf8",
                [
                    "kr. 1.234.567,89",
                    "kr. -1,50",
                    "kr. 0,00",
                    "",
                    "kr. -92.233.720.368.547.758,08",
                ],
            ),
        ] {
            let path = snail_under(&format!("money-{locale}"), locale).unwrap();
            let mut s = Snail::open(&path).unwrap();
            for order in ["t_pkey", "t_v"] {
                let (b, _) = s
                    .ask("SELECT t.id, t.v FROM s.t t ORDER BY t.id", Some(order))
                    .unwrap();
                assert_eq!(shown(&b, 1), some(&want), "{locale} along {order}");
                assert_eq!(
                    b.schema().field(1).metadata().get("lc_monetary"),
                    Some(&locale.to_string())
                );
            }
        }
    }

    #[test]
    fn a_literal_beside_money_is_read_under_the_file_s_locale_on_both_paths() {
        let path = snail_under("money-dk-literals", "en_DK.utf8").unwrap();
        let mut s = Snail::open(&path).unwrap();
        for (w, want) in [
            ("v = 'kr. -1,50'", vec![2]),
            ("v = '-1,5'", vec![2]),
            ("v < '0'", vec![2, 5]),
            ("v >= '1.234.567,89'", vec![1]),
            ("v = 'kr. 1.234.567,889'", vec![1]),
            ("v = CAST('-1,5' AS money)", vec![2]),
        ] {
            let (b, _) = s
                .ask(
                    &format!("SELECT t.id FROM s.t t WHERE t.{w} ORDER BY t.id"),
                    None,
                )
                .unwrap();
            assert_eq!(ids(&b), want, "{w}");
            let (b, _) = s
                .ask(
                    &format!(
                        "WITH x AS (SELECT id, v FROM s.t) SELECT id FROM x WHERE {w} ORDER BY id"
                    ),
                    None,
                )
                .unwrap();
            assert_eq!(ids(&b), want, "in a pipeline, {w}");
        }
        let e = s
            .ask("SELECT t.id FROM s.t t WHERE t.v = '$1.00'", None)
            .unwrap_err()
            .to_string();
        assert!(e.contains("invalid input syntax for type money"), "{e}");
    }

    #[test]
    fn every_money_value_a_statement_makes_reads_and_prints_under_the_file_s_locale() {
        let path = snail_under("money-dk-made", "en_DK.utf8").unwrap();
        let mut s = Snail::open(&path).unwrap();
        // Each as PostgreSQL 18.6 gave it on 28818 under lc_monetary en_DK.utf8 and lc_numeric C.
        let (b, _) = s
            .ask(
                "WITH x AS (SELECT id, v FROM s.t) SELECT id, v::text AS a, \
                 CAST('2,255' AS money)::text AS b, (v * 2)::text AS c, \
                 CAST(v AS numeric)::text AS d, CAST(-150 AS money)::text AS e, \
                 to_char(1234.5, 'L9G999D99') AS f, to_char(-1234.5, 'L9G999D99') AS g \
                 FROM x WHERE id = 2",
                None,
            )
            .unwrap();
        let row: Vec<Option<String>> = (1..b.num_columns())
            .map(|c| shown(&b, c)[0].clone())
            .collect();
        assert_eq!(
            row,
            some(&[
                "kr. -1,50",
                "kr. 2,26",
                "kr. -3,00",
                "-1.50",
                "kr. -150,00",
                "kr. 1,234.50",
                "kr.-1,234.50"
            ])
        );
        let (b, _) = s
            .ask("SELECT sum(t.v) AS m FROM s.t t WHERE t.id < 4", None)
            .unwrap();
        assert_eq!(shown(&b, 0), some(&["kr. 1.234.566,39"]));
    }

    #[test]
    #[cfg(feature = "standalone")]
    fn a_write_reads_its_money_and_set_takes_the_file_s_lc_monetary_alone() {
        let path = snail_under("money-il-writes", "en_IL.utf8").unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.t (id, v) VALUES (6, '₪ 2.50-'), (7, '(3)')")
            .unwrap();
        w.execute("SET lc_monetary = 'en_IL.utf8'").unwrap();
        w.execute("SET lc_monetary = DEFAULT").unwrap();
        let e = w.execute("SET lc_monetary = 'C'").unwrap_err().to_string();
        assert!(e.contains("lc_monetary en_IL.utf8"), "{e}");
        let (b, _) = w
            .ask(
                "SELECT t.id, t.v FROM s.t t WHERE t.id > 5 ORDER BY t.id",
                None,
            )
            .unwrap();
        assert_eq!(shown(&b, 1), some(&["₪ 2.50-", "₪ 3.00-"]));
        // A table made on the snail takes the file's lc_monetary for its money.
        w.execute("CREATE TABLE s.u (id integer NOT NULL, m money, PRIMARY KEY (id))")
            .unwrap();
        w.execute("INSERT INTO s.u (id, m) VALUES (1, '₪ 1,000.5')")
            .unwrap();
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        assert!(s.monetary().iter().all(|(_, _, l)| l == "en_IL.utf8"));
        let (b, _) = s.ask("SELECT u.id, u.m FROM s.u u", None).unwrap();
        assert_eq!(shown(&b, 1), some(&["₪ 1,000.50"]));
    }

    #[test]
    fn money_of_two_locales_or_of_one_glibc_lacks_is_refused_by_name() {
        let ddl = "CREATE TABLE s.t (id integer NOT NULL, v money, w money, PRIMARY KEY (id));";
        let e = write_carried(
            &tmp("money-two.snail"),
            &Catalog::parse(ddl).unwrap(),
            &[],
            &[],
            span(),
            warren_snail::regs::LATEST,
            &carried(&[("s.t", "v", "en_GB.utf8"), ("s.t", "w", "en_IL.utf8")]),
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("the snail answers under one"), "{e}");
        let e = snail_under("money-xx", "xx_YY.utf8")
            .unwrap_err()
            .to_string();
        assert!(
            e.contains("invalid value for parameter \"lc_monetary\": \"xx_YY.utf8\""),
            "{e}"
        );
        // C and POSIX are one locale.
        write_carried(
            &tmp("money-c-posix.snail"),
            &Catalog::parse(ddl).unwrap(),
            &[],
            &[],
            span(),
            warren_snail::regs::LATEST,
            &carried(&[("s.t", "v", "C"), ("s.t", "w", "POSIX")]),
        )
        .unwrap();
    }

    #[test]
    fn out_names_the_lc_monetary_and_a_recut_carries_it() {
        let path = snail_under("money-gb-out", "en_GB.utf8").unwrap();
        let mut s = Snail::open(&path).unwrap();
        let dir = tmp("money-gb-parquet");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let outs = s.out(&dir).unwrap();
        let file = &outs.iter().find(|t| t.table == "s.t").unwrap().file;
        let reader =
            parquet::file::reader::SerializedFileReader::new(std::fs::File::open(file).unwrap())
                .unwrap();
        let meta = parquet::file::reader::FileReader::metadata(&reader).clone();
        let named = meta
            .file_metadata()
            .key_value_metadata()
            .unwrap()
            .iter()
            .find(|k| k.key == warren_snail::read::MONETARY_KEY)
            .and_then(|k| k.value.clone());
        assert_eq!(named.as_deref(), Some("en_GB.utf8"));
        let to = tmp("money-gb-recut.snail");
        Snail::open(&path).unwrap().recut(&to).unwrap();
        let mut r = Snail::open(&to).unwrap();
        let (b, _) = r
            .ask("SELECT t.id, t.v FROM s.t t WHERE t.id = 2", None)
            .unwrap();
        assert_eq!(shown(&b, 1), some(&["-£1.50"]));
    }

    #[test]
    #[cfg(feature = "standalone")]
    fn money_held_in_an_array_reads_and_prints_under_the_file_s_locale() {
        // A file whose money lies in an array alone records it for that column.
        let ddl = "CREATE TABLE s.a (id integer NOT NULL, a money[], PRIMARY KEY (id));";
        let path = tmp("money-dk-array.snail");
        write_carried(
            &path,
            &Catalog::parse(ddl).unwrap(),
            &[],
            &[],
            span(),
            warren_snail::regs::LATEST,
            &carried(&[("s.a", "a", "en_DK.utf8")]),
        )
        .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.a (id, a) VALUES (1, '{1.5,-2}')")
            .unwrap();
        // Each as PostgreSQL 18.6 printed it on 28818 under lc_monetary en_DK.utf8 (2026-10-31).
        let (b, _) = w
            .ask(
                "WITH x AS (SELECT id, a FROM s.a) SELECT id, a::text AS t, \
                 CAST('{1.5,-2}' AS money[])::text AS u FROM x",
                None,
            )
            .unwrap();
        assert_eq!(
            [shown(&b, 1), shown(&b, 2)].concat(),
            some(&[
                "{\"kr. 15,00\",\"kr. -2,00\"}",
                "{\"kr. 15,00\",\"kr. -2,00\"}"
            ])
        );
    }

    #[test]
    #[cfg(feature = "standalone")]
    fn a_domain_s_check_and_a_default_read_their_money_under_the_file_s_locale() {
        // Each as PostgreSQL 18.6 and 19 RC1 took, refused and printed it on 28818 and 28819 under
        // lc_monetary en_DK.utf8 (2026-10-31): a money literal standing alone, read where the
        // domain's CHECK and the DEFAULT are.
        let ddl = "CREATE DOMAIN s.floor AS money CHECK (VALUE >= 'kr. 1,50'::money);
                   CREATE TABLE s.d (id integer NOT NULL, v s.floor,
                       w money DEFAULT 'kr. 1,50'::money + CAST(1 AS money), PRIMARY KEY (id));";
        let path = tmp("money-dk-domain.snail");
        write_carried(
            &path,
            &Catalog::parse(ddl).unwrap(),
            &[],
            &[],
            span(),
            warren_snail::regs::LATEST,
            &carried(&[("s.d", "v", "en_DK.utf8"), ("s.d", "w", "en_DK.utf8")]),
        )
        .unwrap();
        let mut w = Snail::open_writing(&path).unwrap();
        w.execute("INSERT INTO s.d (id, v) VALUES (1, CAST(2 AS money))")
            .unwrap();
        w.execute("INSERT INTO s.d (id, v) VALUES (3, 'kr. 1,50')")
            .unwrap();
        let e = w
            .execute("INSERT INTO s.d (id, v) VALUES (2, CAST(1 AS money))")
            .unwrap_err()
            .to_string();
        assert_eq!(
            e,
            "value for domain s.floor violates check constraint \"floor_check\""
        );
        drop(w);
        let mut s = Snail::open(&path).unwrap();
        let (b, _) = s
            .ask("SELECT d.id, d.v, d.w FROM s.d d ORDER BY d.id", None)
            .unwrap();
        assert_eq!(
            [shown(&b, 1), shown(&b, 2)].concat(),
            some(&["kr. 2,00", "kr. 1,50", "kr. 2,50", "kr. 2,50"])
        );
    }
}
