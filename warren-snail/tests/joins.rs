//! Questions on three tables and more, outer joins, self-joins and joins on inequalities, asked of a
//! snail of themes, their sets, the sets builders own and what they bought: each answer against the
//! rows worked out from the tables directly, each table's read against the pages its directory
//! predicted, and the order the tables were read in.

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::{Round, Snail};
use warren_snail::values::Values;
use warren_snail::write::Rows;

const DDL: &str = "CREATE SCHEMA j; 
CREATE TABLE j.themes (
    theme_id integer NOT NULL,
    parent_id integer,
    name text NOT NULL,
    PRIMARY KEY (theme_id)
);
CREATE TABLE j.sets (
    set_num text NOT NULL,
    theme_id integer NOT NULL,
    year integer NOT NULL,
    PRIMARY KEY (set_num)
);
CREATE INDEX s_theme ON j.sets (theme_id, set_num);
CREATE TABLE j.owned (
    builder_id integer NOT NULL,
    row_no integer NOT NULL,
    set_num text NOT NULL,
    PRIMARY KEY (builder_id, row_no)
);
CREATE INDEX o_set ON j.owned (set_num, builder_id, row_no);
CREATE TABLE j.buys (
    buy_id bigint NOT NULL,
    builder_id integer NOT NULL,
    row_no integer NOT NULL,
    paid integer,
    PRIMARY KEY (buy_id)
);
CREATE INDEX b_row ON j.buys (builder_id, row_no);
ALTER TABLE j.buys CLUSTER ON b_row;
CREATE TABLE j.spans (
    span_id integer NOT NULL,
    lo integer NOT NULL,
    hi integer NOT NULL,
    PRIMARY KEY (span_id)
);
";

struct Theme {
    id: i64,
    parent: Option<i64>,
    name: String,
}

struct Set {
    num: String,
    theme: i64,
    year: i64,
}

struct Owned {
    builder: i64,
    row: i64,
    set: String,
}

struct Buy {
    id: i64,
    builder: i64,
    row: i64,
    paid: Option<i64>,
}

struct Data {
    themes: Vec<Theme>,
    sets: Vec<Set>,
    owned: Vec<Owned>,
    buys: Vec<Buy>,
    spans: Vec<(i64, i64, i64)>,
}

/// Themes 1 to 60, 1 to 10 the roots; 3,000 sets, of themes 1 to 50 and of 61 to 70, which no theme
/// is; 12,000 rows owned, of all but the last 300 sets; 40,000 buys, one in twenty of a row nobody
/// owns, every seventh paid nothing; 2,000 spans.
fn data() -> Data {
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 17
    };
    let themes = (1..=60)
        .map(|id| Theme {
            id,
            parent: (id > 10).then_some(1 + id % 10),
            name: format!("theme {id:02}"),
        })
        .collect();
    let sets: Vec<Set> = (0..3000)
        .map(|i| {
            let t = 1 + (next() % 60) as i64;
            Set {
                num: format!("{}-1", 10_000 + i * 3),
                theme: if t > 50 { t + 10 } else { t },
                year: 1990 + (next() % 30) as i64,
            }
        })
        .collect();
    let mut rows: HashMap<i64, i64> = HashMap::new();
    let owned: Vec<Owned> = (0..12_000)
        .map(|_| {
            let builder = 1 + (next() % 2000) as i64;
            let row = rows.entry(builder).or_default();
            *row += 1;
            Owned {
                builder,
                row: *row,
                set: sets[(next() % 2700) as usize].num.clone(),
            }
        })
        .collect();
    let buys = (0..40_000)
        .map(|i| {
            let (builder, row) = if next() % 20 == 0 {
                (3000 + (next() % 100) as i64, 1)
            } else {
                let o = &owned[(next() % owned.len() as u64) as usize];
                (o.builder, o.row)
            };
            Buy {
                id: i * 3 + 1,
                builder,
                row,
                paid: (i % 7 != 0).then(|| (next() % 500) as i64),
            }
        })
        .collect();
    let spans = (0..2000)
        .map(|i| {
            let lo = (next() % 100_000) as i64;
            (i, lo, lo + 1 + (next() % 300) as i64)
        })
        .collect();
    Data {
        themes,
        sets,
        owned,
        buys,
        spans,
    }
}

fn ints(v: impl Iterator<Item = i64>) -> Values {
    Values::Int(v.map(Some).collect())
}

fn texts(v: impl Iterator<Item = String>) -> Values {
    Values::Text(v.map(Some).collect())
}

fn snail(name: &str, d: &Data) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.snail"));
    let _ = std::fs::remove_file(&path);
    let span = std::env::var("SNAIL_SPAN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(warren_snail::file::FILE_PAGES);
    let tables = vec![
        (
            "j.themes".to_string(),
            Rows {
                columns: vec![
                    ints(d.themes.iter().map(|t| t.id)),
                    Values::Int(d.themes.iter().map(|t| t.parent).collect()),
                    texts(d.themes.iter().map(|t| t.name.clone())),
                ],
            },
        ),
        (
            "j.sets".to_string(),
            Rows {
                columns: vec![
                    texts(d.sets.iter().map(|s| s.num.clone())),
                    ints(d.sets.iter().map(|s| s.theme)),
                    ints(d.sets.iter().map(|s| s.year)),
                ],
            },
        ),
        (
            "j.owned".to_string(),
            Rows {
                columns: vec![
                    ints(d.owned.iter().map(|o| o.builder)),
                    ints(d.owned.iter().map(|o| o.row)),
                    texts(d.owned.iter().map(|o| o.set.clone())),
                ],
            },
        ),
        (
            "j.buys".to_string(),
            Rows {
                columns: vec![
                    ints(d.buys.iter().map(|b| b.id)),
                    ints(d.buys.iter().map(|b| b.builder)),
                    ints(d.buys.iter().map(|b| b.row)),
                    Values::Int(d.buys.iter().map(|b| b.paid).collect()),
                ],
            },
        ),
        (
            "j.spans".to_string(),
            Rows {
                columns: vec![
                    ints(d.spans.iter().map(|s| s.0)),
                    ints(d.spans.iter().map(|s| s.1)),
                    ints(d.spans.iter().map(|s| s.2)),
                ],
            },
        ),
    ];
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_spanned(&path, &catalog, &tables, &[], span).unwrap();
    path
}

/// A snail holding no pages between rounds, so every round reads what it predicts from the file.
fn fresh(path: &Path) -> Snail {
    #[cfg(feature = "standalone")]
    return Snail::open_holding(path, 0).unwrap();
    #[cfg(not(feature = "standalone"))]
    Snail::open(path).unwrap()
}

type Row = Vec<Option<String>>;

/// An answer's rows, each value as text, NULL as none.
fn rows(b: &RecordBatch) -> Vec<Row> {
    (0..b.num_rows())
        .map(|r| {
            b.columns()
                .iter()
                .map(|c| (!c.is_null(r)).then(|| array_value_to_string(c, r).unwrap()))
                .collect()
        })
        .collect()
}

fn s(v: impl ToString) -> Option<String> {
    Some(v.to_string())
}

/// The question's answer, every row as the rows worked out from the tables, in any order; each
/// table's pages read as predicted, less those held.
fn same(snail: &mut Snail, q: &str, mut want: Vec<Row>) -> Round {
    let (b, round) = snail.ask(q, None).unwrap_or_else(|e| panic!("{q}: {e}"));
    let mut got = rows(&b);
    got.sort();
    want.sort();
    assert_eq!(got.len(), want.len(), "{q}: {round:?}");
    assert_eq!(got, want, "{q}");
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
    round
}

/// The pages of the columns `columns` of an order, over every block.
fn whole(snail: &Snail, order: &str, columns: &[&str]) -> u64 {
    snail
        .blocks()
        .unwrap()
        .iter()
        .filter(|r| r.order == order && columns.contains(&r.column.as_str()))
        .map(|r| r.disk_pages)
        .sum()
}

fn exact_of(round: &Round, order: &str) -> u64 {
    round
        .consulted
        .iter()
        .filter(|c| c.order == order)
        .filter_map(|c| c.exact)
        .min()
        .unwrap()
}

#[test]
fn three_tables_and_four_are_read_from_their_scope_outward() {
    let d = data();
    let path = snail("joins_chain", &d);
    let mut snail = fresh(&path);
    let theme = 7;
    let sets: BTreeSet<&str> = d
        .sets
        .iter()
        .filter(|s| s.theme == theme)
        .map(|s| s.num.as_str())
        .collect();

    // A theme's sets, each with how many builders own it: the theme, then its sets kept to its key,
    // then the rows owned kept to the sets' keys, along the strand led by the set.
    let mut owned: BTreeMap<&str, i64> = BTreeMap::new();
    for o in d.owned.iter().filter(|o| sets.contains(o.set.as_str())) {
        *owned.entry(o.set.as_str()).or_default() += 1;
    }
    let want: Vec<Row> = owned.iter().map(|(k, n)| vec![s(k), s(n)]).collect();
    let q = format!(
        "SELECT s.set_num, count(*) FROM j.themes t \
         JOIN j.sets s ON s.theme_id = t.theme_id \
         JOIN j.owned o ON o.set_num = s.set_num \
         WHERE t.theme_id = {theme} GROUP BY s.set_num"
    );
    let round = same(&mut snail, &q, want.clone());
    assert_eq!(round.order, "themes_pkey+s_theme+o_set", "{round:?}");
    assert!(
        exact_of(&round, "o_set") < whole(&snail, "o_set", &["set_num", "builder_id", "row_no"]),
        "the rows owned are kept to the theme's sets: {round:?}"
    );
    // Written the other way round, the scope is still read first.
    let q = format!(
        "SELECT s.set_num, count(*) FROM j.owned o \
         JOIN j.sets s ON o.set_num = s.set_num \
         JOIN j.themes t ON t.theme_id = s.theme_id \
         WHERE t.theme_id = {theme} GROUP BY s.set_num"
    );
    let round = same(&mut snail, &q, want);
    assert_eq!(round.order, "themes_pkey+s_theme+o_set", "{round:?}");

    // Four tables: what the theme's builders bought, by set, the buys kept to the rows owned.
    let rows: BTreeSet<(i64, i64)> = d
        .owned
        .iter()
        .filter(|o| sets.contains(o.set.as_str()))
        .map(|o| (o.builder, o.row))
        .collect();
    let set_of: HashMap<(i64, i64), &str> = d
        .owned
        .iter()
        .map(|o| ((o.builder, o.row), o.set.as_str()))
        .collect();
    let mut bought: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for b in d.buys.iter().filter(|b| rows.contains(&(b.builder, b.row))) {
        let e = bought.entry(set_of[&(b.builder, b.row)]).or_default();
        e.0 += 1;
        e.1 += b.paid.unwrap_or(0);
    }
    let want: Vec<Row> = bought
        .iter()
        .map(|(k, (n, p))| vec![s(k), s(n), s(p)])
        .collect();
    let q = format!(
        "SELECT s.set_num, count(*), sum(b.paid) FROM j.themes t \
         JOIN j.sets s ON s.theme_id = t.theme_id \
         JOIN j.owned o ON o.set_num = s.set_num \
         JOIN j.buys b ON b.builder_id = o.builder_id AND b.row_no = o.row_no \
         WHERE t.theme_id = {theme} GROUP BY s.set_num"
    );
    let round = same(&mut snail, &q, want.clone());
    assert_eq!(round.order, "themes_pkey+s_theme+o_set+b_row", "{round:?}");
    assert!(
        exact_of(&round, "b_row") < whole(&snail, "b_row", &["builder_id", "row_no", "paid"]),
        "the buys are kept to the rows owned: {round:?}"
    );
    // The keys written the other way round in the ON: the buys still kept along their order.
    let q = format!(
        "SELECT s.set_num, count(*), sum(b.paid) FROM j.buys b \
         JOIN j.owned o ON o.row_no = b.row_no AND o.builder_id = b.builder_id \
         JOIN j.sets s ON o.set_num = s.set_num \
         JOIN j.themes t ON s.theme_id = t.theme_id \
         WHERE t.theme_id = {theme} GROUP BY s.set_num"
    );
    let round = same(&mut snail, &q, want);
    assert_eq!(round.order, "themes_pkey+s_theme+o_set+b_row", "{round:?}");

    // Nothing fixed: the first table as written, then along the chain.
    let total = d
        .buys
        .iter()
        .filter(|b| set_of.contains_key(&(b.builder, b.row)))
        .count();
    let q = "SELECT count(*) FROM j.buys b \
             JOIN j.owned o ON o.builder_id = b.builder_id AND o.row_no = b.row_no \
             JOIN j.sets s ON s.set_num = o.set_num";
    let round = same(&mut snail, q, vec![vec![s(total)]]);
    assert!(
        round.order.starts_with("b_row+") || round.order.starts_with("buys_pkey+"),
        "{round:?}"
    );

    // Two scopes, at both ends of the chain: the theme and the buys of one builder.
    let builder = d.buys[0].builder;
    let n = d
        .buys
        .iter()
        .filter(|b| b.builder == builder && rows.contains(&(b.builder, b.row)))
        .count();
    let q = format!(
        "SELECT count(*) FROM j.themes t \
         JOIN j.sets s ON s.theme_id = t.theme_id \
         JOIN j.owned o ON o.set_num = s.set_num \
         JOIN j.buys b ON b.builder_id = o.builder_id AND b.row_no = o.row_no \
         WHERE t.theme_id = {theme} AND b.builder_id = {builder}"
    );
    let round = same(&mut snail, &q, vec![vec![s(n)]]);
    assert_eq!(
        round.order, "themes_pkey+b_row+owned_pkey+sets_pkey",
        "{round:?}"
    );
}

#[test]
fn an_outer_join_keeps_its_driving_side_and_reads_the_other_kept_to_its_keys() {
    let d = data();
    let path = snail("joins_outer", &d);
    let mut snail = fresh(&path);
    let theme = 3;
    let mut want: Vec<Row> = Vec::new();
    for st in d.sets.iter().filter(|st| st.theme == theme) {
        let mine: Vec<&Owned> = d.owned.iter().filter(|o| o.set == st.num).collect();
        if mine.is_empty() {
            want.push(vec![s(&st.num), None, None]);
        }
        for o in mine {
            want.push(vec![s(&st.num), s(o.builder), s(o.row)]);
        }
    }
    let q = format!(
        "SELECT s.set_num, o.builder_id, o.row_no FROM j.sets s \
         LEFT JOIN j.owned o ON o.set_num = s.set_num WHERE s.theme_id = {theme}"
    );
    let round = same(&mut snail, &q, want.clone());
    assert_eq!(round.order, "s_theme+o_set", "{round:?}");
    assert!(want.iter().any(|r| r[1].is_none()), "a set nobody owns");
    assert!(
        exact_of(&round, "o_set") < whole(&snail, "o_set", &["set_num", "builder_id", "row_no"]),
        "the table a LEFT JOIN may leave NULL is kept to the keys of the table it keeps: {round:?}"
    );
    // A RIGHT JOIN keeps the table after it.
    let q = format!(
        "SELECT s.set_num, o.builder_id, o.row_no FROM j.owned o \
         RIGHT JOIN j.sets s ON o.set_num = s.set_num WHERE s.theme_id = {theme}"
    );
    let round = same(&mut snail, &q, want.clone());
    assert_eq!(round.order, "s_theme+o_set", "{round:?}");
    // A condition on the table it may leave NULL, in its ON, narrows that table alone.
    let builders: BTreeSet<i64> = (1..=600).collect();
    let mut narrowed: Vec<Row> = Vec::new();
    for st in d.sets.iter().filter(|st| st.theme == theme) {
        let mine: Vec<&Owned> = d
            .owned
            .iter()
            .filter(|o| o.set == st.num && builders.contains(&o.builder))
            .collect();
        if mine.is_empty() {
            narrowed.push(vec![s(&st.num), None]);
        }
        for o in mine {
            narrowed.push(vec![s(&st.num), s(o.builder)]);
        }
    }
    let q = format!(
        "SELECT s.set_num, o.builder_id FROM j.sets s \
         LEFT JOIN j.owned o ON o.set_num = s.set_num AND o.builder_id <= 600 \
         WHERE s.theme_id = {theme}"
    );
    let round = same(&mut snail, &q, narrowed);
    assert_eq!(
        round.order, "s_theme+owned_pkey",
        "its builders narrow the rows owned: {round:?}"
    );
    // A condition on the table it keeps, in its ON, joins only its rows that pass, and keeps the rest.
    let mut kept: Vec<Row> = Vec::new();
    for st in d.sets.iter().filter(|st| st.theme == theme) {
        let mine: Vec<&Owned> = if st.year >= 2005 {
            d.owned.iter().filter(|o| o.set == st.num).collect()
        } else {
            Vec::new()
        };
        if mine.is_empty() {
            kept.push(vec![s(&st.num), None]);
        }
        for o in mine {
            kept.push(vec![s(&st.num), s(o.builder)]);
        }
    }
    let q = format!(
        "SELECT s.set_num, o.builder_id FROM j.sets s \
         LEFT JOIN j.owned o ON o.set_num = s.set_num AND s.year >= 2005 \
         WHERE s.theme_id = {theme}"
    );
    same(&mut snail, &q, kept);

    // WHERE testing the table it may leave NULL for NULL: the sets nobody owns.
    let owned: BTreeSet<&str> = d.owned.iter().map(|o| o.set.as_str()).collect();
    let want: Vec<Row> = d
        .sets
        .iter()
        .filter(|st| !owned.contains(st.num.as_str()))
        .map(|st| vec![s(&st.num)])
        .collect();
    assert!(want.len() >= 300, "{}", want.len());
    let q = "SELECT s.set_num FROM j.sets s LEFT JOIN j.owned o ON o.set_num = s.set_num \
             WHERE o.set_num IS NULL";
    same(&mut snail, q, want);
    // The buys of rows nobody owns: the left outer join's null side.
    let rows: BTreeSet<(i64, i64)> = d.owned.iter().map(|o| (o.builder, o.row)).collect();
    let want: Vec<Row> = d
        .buys
        .iter()
        .filter(|b| !rows.contains(&(b.builder, b.row)))
        .map(|b| vec![s(b.id)])
        .collect();
    let q = "SELECT b.buy_id FROM j.buys b \
             LEFT JOIN j.owned o ON o.builder_id = b.builder_id AND o.row_no = b.row_no \
             WHERE o.builder_id IS NULL";
    same(&mut snail, q, want);

    // WHERE comparing the table it may leave NULL makes the outer join an inner one, which may then
    // read that table first.
    let set_of: HashMap<(i64, i64), &str> = d
        .owned
        .iter()
        .map(|o| ((o.builder, o.row), o.set.as_str()))
        .collect();
    let want: Vec<Row> = d
        .buys
        .iter()
        .filter(|b| b.builder == 77)
        .filter_map(|b| {
            set_of
                .get(&(b.builder, b.row))
                .map(|st| vec![s(b.id), s(st)])
        })
        .collect();
    let q = "SELECT b.buy_id, o.set_num FROM j.buys b \
             LEFT JOIN j.owned o ON o.builder_id = b.builder_id AND o.row_no = b.row_no \
             WHERE o.builder_id = 77";
    let round = same(&mut snail, q, want);
    assert!(
        round.order.starts_with("owned_pkey+"),
        "an inner join, the builder fixes the rows owned first: {round:?}"
    );

    // A chain of outer joins: a theme's sets, their rows owned, and the buys of each.
    let theme_sets: Vec<&Set> = d.sets.iter().filter(|st| st.theme == theme).collect();
    let mut want: Vec<Row> = Vec::new();
    for st in &theme_sets {
        let mine: Vec<&Owned> = d.owned.iter().filter(|o| o.set == st.num).collect();
        if mine.is_empty() {
            want.push(vec![s(&st.num), None, None]);
        }
        for o in mine {
            let b: Vec<&Buy> = d
                .buys
                .iter()
                .filter(|b| b.builder == o.builder && b.row == o.row)
                .collect();
            if b.is_empty() {
                want.push(vec![s(&st.num), s(o.builder), None]);
            }
            for b in b {
                want.push(vec![s(&st.num), s(o.builder), s(b.id)]);
            }
        }
    }
    let q = format!(
        "SELECT s.set_num, o.builder_id, b.buy_id FROM j.sets s \
         LEFT JOIN j.owned o ON o.set_num = s.set_num \
         LEFT JOIN j.buys b ON b.builder_id = o.builder_id AND b.row_no = o.row_no \
         WHERE s.theme_id = {theme}"
    );
    let round = same(&mut snail, &q, want);
    assert_eq!(round.order, "s_theme+o_set+b_row", "{round:?}");

    // Counted: each theme's sets give the answer nothing but their number, a theme with none one
    // row.
    let want: Vec<Row> = d
        .themes
        .iter()
        .map(|t| {
            let n = d.sets.iter().filter(|st| st.theme == t.id).count().max(1);
            vec![s(t.id), s(n)]
        })
        .collect();
    let q = "SELECT t.theme_id, count(*) FROM j.themes t LEFT JOIN j.sets s ON s.theme_id = t.theme_id \
             GROUP BY t.theme_id";
    same(&mut snail, q, want);

    // FULL: every theme with its sets, a theme with none, and a set of no theme.
    let ids: BTreeSet<i64> = d.themes.iter().map(|t| t.id).collect();
    let mut want: Vec<Row> = Vec::new();
    for t in &d.themes {
        let mine: Vec<&Set> = d.sets.iter().filter(|st| st.theme == t.id).collect();
        if mine.is_empty() {
            want.push(vec![s(t.id), None]);
        }
        for st in mine {
            want.push(vec![s(t.id), s(&st.num)]);
        }
    }
    for st in d.sets.iter().filter(|st| !ids.contains(&st.theme)) {
        want.push(vec![None, s(&st.num)]);
    }
    let q = "SELECT t.theme_id, s.set_num FROM j.themes t FULL JOIN j.sets s ON s.theme_id = t.theme_id";
    same(&mut snail, q, want);
    // A FULL JOIN whose WHERE compares one of its tables keeps only that one.
    let mut want: Vec<Row> = Vec::new();
    for t in d.themes.iter().filter(|t| t.id <= 55) {
        let mine: Vec<&Set> = d.sets.iter().filter(|st| st.theme == t.id).collect();
        if mine.is_empty() {
            want.push(vec![s(t.id), None]);
        }
        for st in mine {
            want.push(vec![s(t.id), s(&st.num)]);
        }
    }
    let q = "SELECT t.theme_id, s.set_num FROM j.themes t FULL JOIN j.sets s ON s.theme_id = t.theme_id \
             WHERE t.theme_id <= 55";
    same(&mut snail, q, want);
}

#[test]
fn a_table_joined_to_itself_and_tables_joined_on_inequalities() {
    let d = data();
    let path = snail("joins_self", &d);
    let mut snail = fresh(&path);
    // Each theme with its parent's name, the roots kept with none.
    let name: HashMap<i64, &str> = d.themes.iter().map(|t| (t.id, t.name.as_str())).collect();
    let want: Vec<Row> = d
        .themes
        .iter()
        .map(|t| vec![s(t.id), t.parent.map(|p| name[&p].to_string())])
        .collect();
    let q = "SELECT t.theme_id, p.name FROM j.themes t LEFT JOIN j.themes p ON p.theme_id = t.parent_id";
    let round = same(&mut snail, q, want.clone());
    assert_eq!(round.order, "themes_pkey+themes_pkey", "{round:?}");
    let inner: Vec<Row> = want.into_iter().filter(|r| r[1].is_some()).collect();
    let q = "SELECT t.theme_id, p.name FROM j.themes t JOIN j.themes p ON p.theme_id = t.parent_id";
    same(&mut snail, q, inner);
    // Grandchildren: a theme, its parent and its parent's parent.
    let q = "SELECT count(*) FROM j.themes t JOIN j.themes p ON p.theme_id = t.parent_id \
             JOIN j.themes g ON g.theme_id = p.parent_id";
    same(&mut snail, q, vec![vec![s(0)]]);

    // Within one theme, each pair of sets the second of which came out later.
    let theme = 5;
    let mine: Vec<&Set> = d.sets.iter().filter(|st| st.theme == theme).collect();
    let pairs = mine
        .iter()
        .flat_map(|a| mine.iter().filter(move |b| a.year < b.year))
        .count();
    let q = format!(
        "SELECT count(*) FROM j.sets a JOIN j.sets b ON b.theme_id = a.theme_id AND a.year < b.year \
         WHERE a.theme_id = {theme}"
    );
    same(&mut snail, &q, vec![vec![s(pairs)]]);
    // The same pairs, each named, and with <> and >=.
    let mut want: Vec<Row> = Vec::new();
    for a in &mine {
        for b in mine.iter().filter(|b| a.year >= b.year && a.num != b.num) {
            want.push(vec![s(&a.num), s(&b.num)]);
        }
    }
    let q = format!(
        "SELECT a.set_num, b.set_num FROM j.sets a JOIN j.sets b ON b.theme_id = a.theme_id \
         AND a.year >= b.year AND a.set_num <> b.set_num WHERE a.theme_id = {theme}"
    );
    same(&mut snail, &q, want);

    // Spans that overlap, each pair once: joined on inequalities alone, both read by their own
    // conditions.
    let some: Vec<&(i64, i64, i64)> = d.spans.iter().filter(|x| x.0 < 600).collect();
    let mut want: Vec<Row> = Vec::new();
    for a in &some {
        for b in d.spans.iter() {
            if a.1 < b.2 && b.1 < a.2 && a.0 < b.0 {
                want.push(vec![s(a.0), s(b.0)]);
            }
        }
    }
    assert!(want.len() > 100, "{}", want.len());
    let q = "SELECT a.span_id, b.span_id FROM j.spans a JOIN j.spans b \
             ON a.lo < b.hi AND b.lo < a.hi AND a.span_id < b.span_id WHERE a.span_id < 600";
    same(&mut snail, q, want);
    // Each span with the spans that start inside it, none where none does.
    let mut want: Vec<Row> = Vec::new();
    for a in d.spans.iter().filter(|x| x.0 < 50) {
        let inside: Vec<&(i64, i64, i64)> =
            d.spans.iter().filter(|b| b.1 > a.1 && b.1 < a.2).collect();
        if inside.is_empty() {
            want.push(vec![s(a.0), None]);
        }
        for b in inside {
            want.push(vec![s(a.0), s(b.0)]);
        }
    }
    let q = "SELECT a.span_id, b.span_id FROM j.spans a LEFT JOIN j.spans b \
             ON b.lo > a.lo AND b.lo < a.hi WHERE a.span_id < 50";
    same(&mut snail, q, want);
}

#[test]
fn a_join_on_text_and_the_tables_named_by_commas_are_read_as_an_inner_join() {
    let d = data();
    let path = snail("joins_comma", &d);
    let mut snail = fresh(&path);
    let theme = 9;
    let sets: BTreeSet<&str> = d
        .sets
        .iter()
        .filter(|s| s.theme == theme)
        .map(|s| s.num.as_str())
        .collect();
    let n = d
        .owned
        .iter()
        .filter(|o| sets.contains(o.set.as_str()))
        .count();
    let q = format!(
        "SELECT count(*) FROM j.themes t, j.sets s, j.owned o \
         WHERE s.theme_id = t.theme_id AND o.set_num = s.set_num AND t.theme_id = {theme}"
    );
    let round = same(&mut snail, &q, vec![vec![s(n)]]);
    assert_eq!(round.order, "themes_pkey+s_theme+o_set", "{round:?}");
    let q = format!(
        "SELECT count(*) FROM j.themes t CROSS JOIN j.sets s JOIN j.owned o ON o.set_num = s.set_num \
         WHERE s.theme_id = t.theme_id AND t.theme_id = {theme}"
    );
    same(&mut snail, &q, vec![vec![s(n)]]);
    // A join on integers of two widths.
    let ids: BTreeSet<i64> = d.spans.iter().map(|x| x.0).collect();
    let n = d.buys.iter().filter(|b| ids.contains(&b.id)).count();
    let q = "SELECT count(*) FROM j.buys b JOIN j.spans s ON s.span_id = b.buy_id";
    same(&mut snail, q, vec![vec![s(n)]]);
}

#[test]
fn what_a_join_does_not_read_is_refused_with_its_reason() {
    let d = data();
    let path = snail("joins_refused", &d);
    let mut snail = fresh(&path);
    for (q, why) in [
        (
            "SELECT count(*) FROM j.themes t FULL JOIN j.sets s ON s.theme_id < t.theme_id",
            "hash-joinable",
        ),
        (
            "SELECT count(*) FROM j.sets s JOIN j.owned o ON o.set_num = s.set_num JOIN j.themes t ON s.year = 3",
            "joined on no key",
        ),
        (
            "SELECT count(*) FROM j.sets s, j.themes t",
            "joined on no key",
        ),
        (
            "SELECT count(*) FROM j.sets s LEFT JOIN j.themes t ON t.name = 'theme 01'",
            "joined on no key",
        ),
        (
            "SELECT count(*) FROM j.sets s JOIN j.owned o ON o.set_num = s.theme_id",
            "through a cast",
        ),
        (
            "SELECT count(*) FROM j.sets s JOIN j.owned o ON o.set_num = s.set_num AND s.year = s.theme_id",
            "one table",
        ),
        (
            "SELECT count(*) FROM j.sets s JOIN j.owned o ON o.set_num = t.name JOIN j.themes t ON t.theme_id = s.theme_id",
            "FROM-clause entry",
        ),
        (
            "SELECT count(*) FROM j.sets s JOIN j.sets s ON s.set_num = s.set_num",
            "more than once",
        ),
        (
            "SELECT theme_id FROM j.sets s JOIN j.themes t ON t.theme_id = s.theme_id",
            "qualify it",
        ),
    ] {
        let e = snail.ask(q, None).unwrap_err().to_string();
        assert!(e.contains(why), "{q}: {e}");
    }
}

/// Two tables of the structured types, written empty.
fn structured(name: &str) -> PathBuf {
    let ddl = "CREATE SCHEMA s; CREATE TABLE s.a (id integer NOT NULL, n inet, j jsonb, t tsvector, x json, p point, \
               b box, PRIMARY KEY (id)); \
               CREATE TABLE s.c (id integer NOT NULL, n cidr, j jsonb, t tsvector, x json, p point, \
               b box, PRIMARY KEY (id));";
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}.snail"));
    let _ = std::fs::remove_file(&path);
    warren_snail::write::write(&path, &Catalog::parse(ddl).unwrap(), &[]).unwrap();
    path
}

#[cfg(feature = "standalone")]
#[test]
fn the_structured_types_join_by_their_classes() {
    let path = structured("joins_structured");
    let mut w = Snail::open_writing(&path).unwrap();
    w.execute(
        "INSERT INTO s.a (id, n, j, t) VALUES (1, '10.1.0.0/16', '{\"a\": 1.0}', 'b a'), \
         (2, '10.1.2.3', '[1, 2]', 'a:1'), (3, '10.1.0.0', '{\"a\": 1}', 'c')",
    )
    .unwrap();
    w.execute(
        "INSERT INTO s.c (id, n, j, t) VALUES (1, '10.1.0.0/16', '{\"a\": 1}', 'a b'), \
         (2, '10.1.2.3/32', '[1,2]', 'a'), (3, '10.2.0.0/16', 'null', 'a:1')",
    )
    .unwrap();
    drop(w);
    let mut snail = fresh(&path);
    // As PostgreSQL 18.6 answers each: an inet beside a cidr by network_eq, jsonb's numbers by
    // their values, a tsvector by its lexemes, an inet before a cidr by network_lt.
    for (on, want) in [
        ("a.n = c.n", vec![(1, 1), (2, 2)]),
        ("a.j = c.j", vec![(1, 1), (2, 2), (3, 1)]),
        ("a.t = c.t", vec![(1, 1), (2, 3)]),
        ("a.n < c.n", vec![(1, 2), (1, 3), (2, 3), (3, 2), (3, 3)]),
    ] {
        let q = format!("SELECT a.id, c.id AS other FROM s.a a JOIN s.c c ON {on}");
        let want: Vec<Row> = want.into_iter().map(|(a, c)| vec![s(a), s(c)]).collect();
        same(&mut snail, &q, want);
    }
}

#[test]
fn a_structured_type_postgresql_has_no_key_for_is_refused_as_postgresql_refuses_it() {
    let path = structured("joins_structured_refused");
    let mut snail = fresh(&path);
    for (on, why) in [
        ("a.x = c.x", "operator does not exist: json = json"),
        ("a.p = c.p", "operator does not exist: point = point"),
        ("a.b = c.b", "compares areas"),
        ("a.n = c.j", "through a cast"),
    ] {
        let q = format!("SELECT count(*) FROM s.a a JOIN s.c c ON {on}");
        let e = snail.ask(&q, None).unwrap_err().to_string();
        assert!(e.contains(why), "{q}: {e}");
    }
}

#[test]
fn a_join_s_forms_past_the_question_path_are_read_by_the_pipeline() {
    let d = data();
    let path = snail("joins_handed", &d);
    let mut snail = fresh(&path);
    let by_builder = |b: i64| d.owned.iter().filter(|o| o.builder == b).count();
    // A RIGHT JOIN after the first join: every buy, beside each pair of a set and a row owned by
    // its builder, or beside NULLs.
    let n: usize = d.buys.iter().map(|b| by_builder(b.builder).max(1)).sum();
    let q = "SELECT count(*) FROM j.sets s JOIN j.owned o ON o.set_num = s.set_num \
             RIGHT JOIN j.buys b ON b.builder_id = o.builder_id";
    same(&mut snail, q, vec![vec![s(n)]]);
    // A FULL JOIN after the first join: each set of a theme beside its rows owned, or NULLs; each
    // row owned of a set of no theme beside NULLs.
    let themed: BTreeSet<&str> = d
        .sets
        .iter()
        .filter(|x| d.themes.iter().any(|t| t.id == x.theme))
        .map(|x| x.num.as_str())
        .collect();
    let owning = |set: &str| d.owned.iter().filter(|o| o.set == set).count();
    let n: usize = themed.iter().map(|x| owning(x).max(1)).sum::<usize>()
        + d.owned
            .iter()
            .filter(|o| !themed.contains(o.set.as_str()))
            .count();
    let q = "SELECT count(*) FROM j.sets s JOIN j.themes t ON t.theme_id = s.theme_id \
             FULL JOIN j.owned o ON o.set_num = s.set_num";
    same(&mut snail, q, vec![vec![s(n)]]);
    // USING and NATURAL: the column they meet on read once, under its bare name.
    let q = "SELECT count(*), count(DISTINCT set_num) FROM j.sets s JOIN j.owned o USING (set_num)";
    let sets: BTreeSet<&str> = d.owned.iter().map(|o| o.set.as_str()).collect();
    same(&mut snail, q, vec![vec![s(d.owned.len()), s(sets.len())]]);
    let q = "SELECT count(*) FROM j.owned o NATURAL JOIN j.buys b";
    let n: usize = d
        .buys
        .iter()
        .filter(|b| {
            d.owned
                .iter()
                .any(|o| o.builder == b.builder && o.row == b.row)
        })
        .count();
    same(&mut snail, q, vec![vec![s(n)]]);
    // Its words, where PostgreSQL refuses: a FULL JOIN on nothing it meets the rows by. Met with the
    // themes, the fewest rows, so that a join read in place of the refusal answers at once.
    let e = snail
        .ask(
            "SELECT count(*) FROM j.sets s JOIN j.themes t ON t.theme_id = s.theme_id \
             FULL JOIN j.themes u ON u.theme_id < s.theme_id",
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(e.contains("hash-joinable"), "{e}");
}

#[test]
fn a_step_reads_its_tables_as_the_question_path_reads_them() {
    let d = data();
    let path = snail("joins_steps", &d);
    let mut snail = fresh(&path);
    let theme = 7;
    let years: Vec<i64> = d
        .sets
        .iter()
        .filter(|x| x.theme == theme)
        .map(|x| x.year)
        .collect();
    // A step reading a join of tables reads it in the question path's one round: the sets kept to
    // the theme's key, along their index on it.
    let joined = format!(
        "SELECT s.set_num, s.year FROM j.themes t JOIN j.sets s ON s.theme_id = t.theme_id \
         WHERE t.theme_id = {theme}"
    );
    let alone = same(
        &mut snail,
        &joined,
        d.sets
            .iter()
            .filter(|x| x.theme == theme)
            .map(|x| vec![s(&x.num), s(x.year)])
            .collect(),
    );
    let q = format!("WITH x AS ({joined}) SELECT count(*), min(x.year) FROM x");
    let round = same(
        &mut snail,
        &q,
        vec![vec![s(years.len()), s(years.iter().min().unwrap())]],
    );
    assert_eq!(round.order, alone.order);
    assert_eq!(round.column_pages_predicted, alone.column_pages_predicted);
    // A table a step reads alone is read by its own conditions: the sets of the theme along their
    // index on it, the rows owned whole; then met by the step.
    let sets = format!("SELECT s.set_num FROM j.sets s WHERE s.theme_id = {theme}");
    let first = same(
        &mut snail,
        &sets,
        d.sets
            .iter()
            .filter(|x| x.theme == theme)
            .map(|x| vec![s(&x.num)])
            .collect(),
    );
    let all: Vec<Row> = d.owned.iter().map(|o| vec![s(&o.set)]).collect();
    let second = same(&mut snail, "SELECT o.set_num FROM j.owned o", all);
    let mine: BTreeSet<&str> = d
        .sets
        .iter()
        .filter(|x| x.theme == theme)
        .map(|x| x.num.as_str())
        .collect();
    let n = d
        .owned
        .iter()
        .filter(|o| mine.contains(o.set.as_str()))
        .count();
    let q = format!(
        "WITH x AS ({sets}), y AS (SELECT o.set_num FROM j.owned o) \
         SELECT count(*) FROM x JOIN y ON y.set_num = x.set_num"
    );
    let round = same(&mut snail, &q, vec![vec![s(n)]]);
    assert_eq!(
        round.column_pages_predicted,
        first.column_pages_predicted + second.column_pages_predicted,
        "{round:?}"
    );
}
