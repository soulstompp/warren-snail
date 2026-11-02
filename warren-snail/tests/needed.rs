//! The columns a question reads of a relation: those it can mean. A name after a relation's name is
//! that relation's alone; a name by itself is that of every relation holding it; a star's, a USING's
//! and a NATURAL's are read by their places. A column too many costs its pages, and one too few
//! comes back NULL: every answer here is PostgreSQL 18.6's over the same rows, in a room that takes
//! it whole and in a small one, and each read's pages are exact.

mod weather;

use arrow::array::{Array, RecordBatch};
use arrow::util::display::array_value_to_string;
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::{begin_together, price_stopped, price_together, Round};
use warren_snail::room::Sizes;
use warren_snail::values::{push_datum, Values};
use warren_snail::write::Rows;
use weather::{fresh, tmp, write};

const DDL: &str = "CREATE TABLE s.a (id bigint NOT NULL, g bigint NOT NULL, name text NOT NULL, \
                   PRIMARY KEY (id));
     CREATE TABLE s.b (id bigint NOT NULL, a_id bigint NOT NULL, v bigint NOT NULL, \
                   PRIMARY KEY (id));
     CREATE INDEX b_a ON s.b (a_id, id) INCLUDE (v);
     CREATE TABLE s.c (id bigint NOT NULL, a bigint NOT NULL, \"Mixed\" bigint NOT NULL, \
                   PRIMARY KEY (id));
     CREATE TABLE s.d (id bigint NOT NULL, w bigint NOT NULL, PRIMARY KEY (id));";

/// A room a round reading these tables whole stops in.
const STOPS: u64 = 64 << 10;

fn table(catalog: &Catalog, name: &str, n: i64, row: impl Fn(i64) -> Vec<String>) -> Rows {
    let t = catalog.table(name).unwrap();
    Rows {
        columns: t
            .columns
            .iter()
            .enumerate()
            .map(|(c, col)| {
                let mut v = Values::new(col.ty);
                for i in 0..n {
                    push_datum(
                        &mut v,
                        &warren_snail::composed::scalar_in(col.ty, &row(i)[c]).unwrap(),
                    );
                }
                v
            })
            .collect(),
    }
}

/// `s.a` of 3,000 rows, `s.b` of 16,384, `s.c` of 1,000 and `s.d` of 3.
fn snail(name: &str) -> PathBuf {
    let path = tmp(&format!("needed-{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    let a = table(&catalog, "s.a", 3_000, |i| {
        vec![i.to_string(), (i % 97).to_string(), format!("name {i}")]
    });
    let b = table(&catalog, "s.b", 16_384, |i| {
        vec![
            i.to_string(),
            (i * 31 % 3_000).to_string(),
            (16_384 - i).to_string(),
        ]
    });
    let c = table(&catalog, "s.c", 1_000, |i| {
        vec![
            i.to_string(),
            (i * 7 % 3_000).to_string(),
            (i % 5).to_string(),
        ]
    });
    let d = table(&catalog, "s.d", 3, |i| {
        vec![i.to_string(), (10 + i).to_string()]
    });
    write(
        &path,
        &catalog,
        &[
            ("s.a".into(), a),
            ("s.b".into(), b),
            ("s.c".into(), c),
            ("s.d".into(), d),
        ],
    )
    .unwrap();
    path
}

/// An answer's rows as text, a NULL `-`, sorted.
fn sorted(batches: &[RecordBatch]) -> Vec<String> {
    let mut rows: Vec<String> = batches
        .iter()
        .flat_map(|b| {
            (0..b.num_rows()).map(|r| {
                b.columns()
                    .iter()
                    .map(|c| match c.is_null(r) {
                        true => "-".to_string(),
                        false => array_value_to_string(c, r).unwrap(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            })
        })
        .collect();
    rows.sort();
    rows
}

/// A question's answer whole, its pages read as predicted.
fn whole(path: &Path, q: &str) -> (Vec<String>, Round) {
    let (b, round) = fresh(path)
        .ask(q, None)
        .unwrap_or_else(|e| panic!("{q}: {e}"));
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
    (sorted(&[b]), round)
}

/// A question's answer in a room of `room` bytes, and the column pages it predicted.
fn stopped(path: &Path, q: &str, room: u64) -> (Vec<String>, u64) {
    let snail = fresh(path);
    let mut a =
        begin_together(&[&snail], q, Sizes::of(room)).unwrap_or_else(|e| panic!("{q}: {e}"));
    let mut batches = Vec::new();
    while let Some(b) = a.next_batch().unwrap_or_else(|e| panic!("{q}: {e}")) {
        batches.push(b);
    }
    (sorted(&batches), a.round().column_pages_predicted)
}

/// A question's answer, whole and in a small room, its rows those written.
fn same(path: &Path, q: &str, want: &[&str]) -> Round {
    let mut want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
    want.sort();
    let (rows, round) = whole(path, q);
    assert_eq!(rows, want, "{q}");
    assert_eq!(stopped(path, q, STOPS).0, want, "{q} in {STOPS}");
    round
}

/// `same`, with the column pages it reads whole.
fn read(path: &Path, q: &str, want: &[&str], pages: u64) {
    let round = same(path, q, want);
    assert_eq!(round.column_pages_predicted, pages, "{q}: {round:?}");
}

fn refused(path: &Path, q: &str) -> String {
    match fresh(path).ask(q, None) {
        Ok((b, _)) => panic!("{q}: answered {:?}", sorted(&[b])),
        Err(e) => e.to_string(),
    }
}

/// The join `rest` measured: `b.id` named by nothing but `a.id`'s word.
const JOINED: &str = "SELECT a.g AS c1, count(*) AS c2 FROM s.a a JOIN s.b b ON b.a_id = a.id \
                      GROUP BY a.g";

#[test]
fn a_column_another_relation_names_alike_is_not_read() {
    let path = snail("alike");
    // Stopped for its room, the pipeline takes the round up.
    let (rows, pages) = stopped(&path, JOINED, STOPS);
    assert_eq!(pages, 86);
    assert_eq!(rows, whole(&path, JOINED).0);
    assert_eq!(rows.len(), 97);
    // Named, it is read, and those are the pages `b.id` costs.
    let named = "SELECT a.g AS c1, count(b.id) AS c2 FROM s.a a JOIN s.b b ON b.a_id = a.id \
                 GROUP BY a.g";
    let (rows, pages) = stopped(&path, named, STOPS);
    assert_eq!(pages, 86 + 47);
    assert_eq!(rows, whole(&path, named).0);
    // Read by the pipeline from the first.
    read(
        &path,
        "SELECT a.g + 1, b.v + 1 FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 3 AND b.v < 500",
        &["4,136", "4,311", "4,389", "4,467", "4,58"],
        24,
    );
    read(
        &path,
        "SELECT a.g + 1, b.v + 1, b.id FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500",
        &[
            "4,136,16249",
            "4,311,16074",
            "4,389,15996",
            "4,467,15918",
            "4,58,16327",
        ],
        26,
    );
}

#[test]
fn a_round_stopped_for_its_room_reads_what_its_price_says_by_the_same_rule() {
    let path = snail("price");
    let snail = fresh(&path);
    let refs = [&snail];
    let most_of = |q: &str, room: u64| {
        let priced = price_together(&refs, q).unwrap();
        let (low, high, _) = priced.column_pages();
        let most = price_stopped(&refs, q, &priced, room)
            .unwrap()
            .column_pages_most;
        assert!(most >= high, "{q}: {most} under {high}");
        (low, most)
    };
    // Aliased, unaliased, and each table going by the other's name.
    for q in [
        JOINED,
        "SELECT a.g AS c1, count(*) AS c2 FROM s.a JOIN s.b ON b.a_id = a.id GROUP BY a.g",
        "SELECT b.g AS c1, count(*) AS c2 FROM s.a b JOIN s.b a ON a.a_id = b.id GROUP BY b.g",
        "SELECT a.id AS c1, b.v AS c2 FROM s.a a LEFT JOIN s.b b ON b.a_id = a.id",
    ] {
        for room in [4u64 << 10, 64 << 10, 1 << 20] {
            let (low, most) = most_of(q, room);
            let (_, pages) = stopped(&path, q, room);
            assert!(
                low <= pages && pages <= most,
                "{q} in {room}: {low} to {most}, {pages}"
            );
        }
    }
    // Its most counts the columns the round reads and no others, however the tables go by their
    // names.
    for q in [
        JOINED,
        "SELECT a.g AS c1, count(*) AS c2 FROM s.a JOIN s.b ON b.a_id = a.id GROUP BY a.g",
        "SELECT s.a.g AS c1, count(*) AS c2 FROM s.a JOIN s.b ON s.b.a_id = s.a.id GROUP BY s.a.g",
        "SELECT b.g AS c1, count(*) AS c2 FROM s.a b JOIN s.b a ON a.a_id = b.id GROUP BY b.g",
    ] {
        assert_eq!(most_of(q, STOPS).1, 132, "{q}");
    }
}

#[test]
fn a_star_reads_the_columns_it_names_and_no_others() {
    let path = snail("star");
    read(
        &path,
        "SELECT * FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        &[
            "780,4,name 780,16380,780,4",
            "811,35,name 811,16381,811,3",
            "842,66,name 842,16382,842,2",
            "873,0,name 873,16383,873,1",
        ],
        38,
    );
    // Each label of its columns is no name of the other relation's.
    read(
        &path,
        "SELECT a.* FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        &[
            "780,4,name 780",
            "811,35,name 811",
            "842,66,name 842",
            "873,0,name 873",
        ],
        36,
    );
    read(
        &path,
        "SELECT b.* FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        &["16380,780,4", "16381,811,3", "16382,842,2", "16383,873,1"],
        16,
    );
    read(
        &path,
        "SELECT a.*, b.v FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        &[
            "780,4,name 780,4",
            "811,35,name 811,3",
            "842,66,name 842,2",
            "873,0,name 873,1",
        ],
        36,
    );
    same(
        &path,
        "SELECT * FROM s.d x, s.d y WHERE x.id = y.id AND x.id < 2",
        &["0,10,0,10", "1,11,1,11"],
    );
    // A star under a relation's name in a subquery is every column of that name's, where a query
    // around it goes by it too.
    read(
        &path,
        "SELECT a.g + 1, b.v FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500 AND EXISTS (SELECT b.* FROM s.c b WHERE b.id = 1)",
        &["4,135", "4,310", "4,388", "4,466", "4,57"],
        30,
    );
}

#[test]
fn a_subquery_going_by_a_name_of_the_query_around_it_reads_its_own() {
    let path = snail("reused");
    // `a.id` and `a.v` are the subquery's own: `s.a`'s has no `v`. `b.v` is the query around it's.
    read(
        &path,
        "SELECT a.name FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 3 AND b.v < 500 \
         AND EXISTS (SELECT 1 FROM s.b a WHERE a.id = b.v AND a.v < 16300)",
        &["name 1458", "name 2719", "name 294", "name 876"],
        38,
    );
    // `b.id` is the subquery's own; the query around it reads `s.b`'s all the same, a column too many
    // and never one too few.
    read(
        &path,
        "SELECT a.g + 1, b.v FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500 AND EXISTS (SELECT b.id FROM s.c b WHERE b.id = 1)",
        &["4,135", "4,310", "4,388", "4,466", "4,57"],
        27,
    );
    same(
        &path,
        "SELECT a.name FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 3 AND b.v < 500 \
         AND EXISTS (SELECT 1 FROM s.c b WHERE b.id = a.g AND b.id = 3)",
        &[
            "name 1458",
            "name 2137",
            "name 2719",
            "name 294",
            "name 876",
        ],
    );
}

#[test]
fn a_column_only_a_subquery_names_is_read_by_the_query_around_it() {
    let path = snail("correlated");
    // Under its relation's name, in WHERE.
    same(
        &path,
        "SELECT a.name FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE EXISTS (SELECT 1 FROM s.c c WHERE c.id = b.id AND c.id < 3)",
        &["name 0", "name 31", "name 62"],
    );
    // By itself.
    same(
        &path,
        "SELECT a.name FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE EXISTS (SELECT 1 FROM s.c c WHERE c.a = v AND c.id < 2)",
        &["name 687"],
    );
    // Through a derived table and a step of the subquery's.
    same(
        &path,
        "SELECT a.name, (SELECT x.y FROM (SELECT a.id AS y) x) FROM s.a a \
         WHERE a.g = 1 AND a.id < 200",
        &["name 1,1", "name 195,195", "name 98,98"],
    );
    same(
        &path,
        "SELECT a.name, (WITH c AS (SELECT a.id AS y) SELECT y FROM c) FROM s.a a \
         WHERE a.g = 1 AND a.id < 200",
        &["name 1,1", "name 195,195", "name 98,98"],
    );
    // In a list, by itself and under its name, a column of its own named alike.
    same(
        &path,
        "SELECT a.name, (SELECT count(*) FROM s.b WHERE a_id = a.id) FROM s.a a WHERE a.id < 2",
        &["name 0,6", "name 1,6"],
    );
    same(
        &path,
        "SELECT g, (SELECT count(*) FROM s.b WHERE a_id = id) FROM s.a a WHERE id < 2",
        &["0,30", "1,30"],
    );
    // LATERAL.
    same(
        &path,
        "SELECT a.g, b.v, x.y FROM s.a a JOIN s.b b ON b.a_id = a.id \
         CROSS JOIN LATERAL (SELECT b.id AS y) x WHERE a.g = 3 AND b.v < 500",
        &[
            "3,135,16249",
            "3,310,16074",
            "3,388,15996",
            "3,466,15918",
            "3,57,16327",
        ],
    );
    same(
        &path,
        "SELECT a.name, l.x FROM s.a a, \
         LATERAL (SELECT b.v AS x FROM s.b b WHERE b.a_id = a.id ORDER BY 1 LIMIT 1) l \
         WHERE a.id < 2",
        &["name 0,1384", "name 1,513"],
    );
    same(
        &path,
        "SELECT a.name, l.x FROM s.a a, LATERAL (SELECT g AS x) l WHERE a.id < 2",
        &["name 0,0", "name 1,1"],
    );
}

#[test]
fn a_whole_row_is_refused_so_no_column_of_it_goes_unread() {
    let path = snail("row");
    for q in [
        "SELECT a FROM s.a a WHERE a.id < 3",
        "SELECT count(a) FROM s.a a",
        "SELECT count(a) FROM s.a a JOIN s.b b ON b.a_id = a.id",
        "SELECT count(a) FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 1000",
        "SELECT row_to_json(a) FROM s.a a WHERE a.id < 3",
        "SELECT to_jsonb(a) FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        "SELECT (a).id FROM s.a a WHERE a.g = 3",
        "SELECT (a).name FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE b.v < 5",
        "SELECT ROW(a.*) FROM s.a a WHERE a.id < 3",
        "SELECT count(a.*) FROM s.a a",
        "SELECT row_to_json(a.*) FROM s.a a WHERE a.id < 3",
        "SELECT a IS NULL FROM s.a a WHERE a.id < 3",
        "SELECT a.id FROM s.a a WHERE a IS NOT NULL AND a.id < 3",
        "SELECT a.name FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a IS NOT NULL AND a.g = 3",
        "SELECT a.id FROM s.a a WHERE a.id < 3 AND EXISTS (SELECT a.*)",
        "SELECT (SELECT count(a) FROM s.b b WHERE b.id < 2) FROM s.a a WHERE a.id < 2",
        "SELECT a.id FROM s.a a WHERE a.id < 3 ORDER BY a.*",
    ] {
        refused(&path, q);
    }
}

#[test]
fn a_using_and_a_natural_read_the_columns_they_meet() {
    let path = snail("met");
    // Named by the USING, whatever the list names.
    same(
        &path,
        "SELECT a.name FROM s.a a JOIN s.b b USING (id) WHERE a.id < 2",
        &["name 0", "name 1"],
    );
    for q in [
        "SELECT id FROM s.a a JOIN s.b b USING (id) WHERE a.g = 3 AND a.id < 200",
        "SELECT id FROM s.a a NATURAL JOIN s.b b WHERE a.g = 3 AND a.id < 200",
    ] {
        same(&path, q, &["100", "197", "3"]);
    }
    same(
        &path,
        "SELECT a.id, b.id FROM s.a a JOIN s.b b USING (id) WHERE a.g = 3 AND a.id < 200",
        &["100,100", "197,197", "3,3"],
    );
    // A star names the merged column by its place, and a relation's star its own.
    for join in ["JOIN s.b b USING (id)", "NATURAL JOIN s.b b"] {
        same(
            &path,
            &format!("SELECT * FROM s.a a {join} WHERE a.id < 2"),
            &["0,0,name 0,0,16384", "1,1,name 1,31,16383"],
        );
        same(
            &path,
            &format!("SELECT a.* FROM s.a a {join} WHERE a.id < 2"),
            &["0,0,name 0", "1,1,name 1"],
        );
        same(
            &path,
            &format!("SELECT b.* FROM s.a a {join} WHERE a.id < 2"),
            &["0,0,16384", "1,31,16383"],
        );
    }
    // A NATURAL names none of the columns it meets, and they are read all the same, however the
    // relations are met.
    same(
        &path,
        "SELECT a.name FROM s.a a NATURAL JOIN s.b b WHERE a.id < 2",
        &["name 0", "name 1"],
    );
    same(
        &path,
        "SELECT b.v FROM s.a a NATURAL JOIN s.b b WHERE b.v < 13387",
        &["13385", "13386"],
    );
    same(
        &path,
        "SELECT a.g, count(*) FROM s.a a NATURAL LEFT JOIN s.c c WHERE a.g = 3 GROUP BY a.g",
        &["3,31"],
    );
    same(
        &path,
        "SELECT b.v, a.g FROM s.b b NATURAL FULL JOIN s.a a WHERE b.id = 3000",
        &["13384,-"],
    );
    same(
        &path,
        "SELECT a.g, b.v FROM s.b b NATURAL FULL JOIN s.a a WHERE a.g = 96 AND a.id < 200",
        &["96,16191", "96,16288"],
    );
    same(
        &path,
        "SELECT a.g, b.v FROM s.a a NATURAL RIGHT JOIN s.b b WHERE b.id = 3",
        &["3,16381"],
    );
    // Met twice, its first meeting's columns read for its second.
    same(
        &path,
        "SELECT a.g, b.v FROM (s.b b NATURAL JOIN s.c c) NATURAL JOIN s.a a \
         WHERE a.g = 96 AND a.id < 200",
        &["96,16191", "96,16288"],
    );
    same(
        &path,
        "SELECT a.g, b.v FROM (s.b b NATURAL JOIN s.c c) NATURAL FULL JOIN s.a a \
         WHERE a.g = 96 AND a.id < 200",
        &["96,16191", "96,16288"],
    );
    same(
        &path,
        "SELECT c.a FROM s.a a NATURAL JOIN s.b b NATURAL JOIN s.c c WHERE c.a < 10",
        &["0", "3", "6", "7"],
    );
    // Beside a derived table and a VALUES, whose side of it no table gives.
    same(
        &path,
        "SELECT b.v FROM (SELECT a.id, a.g FROM s.a a) x NATURAL JOIN s.b b \
         WHERE x.g = 3 AND x.id < 200",
        &["16187", "16284", "16381"],
    );
    same(
        &path,
        "SELECT x.q FROM (VALUES (1, 5), (2, 6)) AS x(id, q) NATURAL JOIN s.a a",
        &["5", "6"],
    );
    same(
        &path,
        "SELECT c.a, x.q FROM s.a a JOIN (VALUES (1, 5), (2, 6)) AS x(id, q) USING (id) \
         JOIN s.c c ON c.id = x.q",
        &["35,5", "42,6"],
    );
}

#[test]
fn a_quoted_name_is_kept_and_a_bare_one_folded() {
    let path = snail("folded");
    for q in [
        "SELECT \"A\".g + 1, b.v FROM s.a \"A\" JOIN s.b b ON b.a_id = \"A\".id \
         WHERE \"A\".g = 3 AND b.v < 500",
        "SELECT A.G + 1, B.V FROM s.a a JOIN s.b b ON B.a_ID = A.ID WHERE A.G = 3 AND b.v < 500",
        "SELECT a.g + 1, b.\"v\" FROM s.a a JOIN s.b b ON b.\"a_id\" = a.\"id\" \
         WHERE a.g = 3 AND b.v < 500",
    ] {
        read(&path, q, &["4,135", "4,310", "4,388", "4,466", "4,57"], 24);
    }
    same(
        &path,
        "SELECT a.g + 1, x.\"Mixed\" FROM s.c x JOIN s.a a ON a.id = x.a WHERE a.g = 3 AND x.id < 200",
        &["4,2", "4,4"],
    );
    same(
        &path,
        "SELECT \"Mixed\" FROM s.c x JOIN s.a a ON a.id = x.a WHERE a.g = 3 AND x.id < 100",
        &["2"],
    );
    // A name folded is no quoted name, and a quoted one no bare one.
    refused(&path, "SELECT x.mixed FROM s.c x");
    refused(&path, "SELECT a.id FROM s.a \"A\" WHERE \"A\".g = 3");
}

#[test]
fn a_relation_goes_by_its_alias_or_its_own_name() {
    let path = snail("alias");
    // Each table going by the other's name: `a.v` is `s.b`'s, and `b.id` `s.a`'s; `s.b`'s `id` unread.
    read(
        &path,
        "SELECT a.v + 1 FROM s.a b JOIN s.b a ON a.a_id = b.id WHERE b.g = 3 AND a.v < 500",
        &["136", "311", "389", "467", "58"],
        24,
    );
    same(
        &path,
        "SELECT a.id, b.name FROM s.a b JOIN s.b a ON a.a_id = b.id WHERE b.g = 3 AND a.v < 500",
        &[
            "15918,name 1458",
            "15996,name 876",
            "16074,name 294",
            "16249,name 2719",
            "16327,name 2137",
        ],
    );
    refused(
        &path,
        "SELECT b.v FROM s.a b JOIN s.b a ON a.a_id = b.id WHERE b.g = 3",
    );
    // No alias: the table's own name, without its schema.
    read(
        &path,
        "SELECT a.g + 1, b.v + 1 FROM s.a JOIN s.b ON b.a_id = a.id WHERE a.g = 3 AND b.v < 500",
        &["4,136", "4,311", "4,389", "4,467", "4,58"],
        24,
    );
    // Aliased, its own name is none of its.
    refused(&path, "SELECT a.id FROM s.a x WHERE x.g = 3");
    // With its schema, as the question path reads it.
    assert_eq!(
        whole(
            &path,
            "SELECT s.a.id FROM s.a WHERE s.a.g = 3 AND s.a.id < 200"
        )
        .0,
        ["100", "197", "3"]
    );
    let (rows, round) = whole(
        &path,
        "SELECT s.a.name, s.b.v FROM s.a JOIN s.b ON s.b.a_id = s.a.id \
         WHERE s.a.g = 3 AND s.b.v < 500",
    );
    assert_eq!(
        rows,
        [
            "name 1458,466",
            "name 2137,57",
            "name 2719,135",
            "name 294,310",
            "name 876,388"
        ]
    );
    assert_eq!(round.column_pages_predicted, 36);
    refused(&path, "SELECT s.a.id FROM s.a x WHERE x.g = 3");
}

#[test]
fn an_output_name_reads_what_it_labels_and_counts_as_a_name_where_it_is_read() {
    let path = snail("output");
    // A label alone names no column, however it is spelled.
    read(
        &path,
        "SELECT b.v + 1 AS id, a.name FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500",
        &[
            "136,name 2719",
            "311,name 294",
            "389,name 876",
            "467,name 1458",
            "58,name 2137",
        ],
        36,
    );
    same(
        &path,
        "SELECT CAST(a.g AS bigint) + 1 AS id FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500",
        &["4", "4", "4", "4", "4"],
    );
    // ORDER BY a name alone: the label's, and read as any name alone is.
    read(
        &path,
        "SELECT a.g + 1 AS id, b.v FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500 ORDER BY id, b.v",
        &["4,135", "4,310", "4,388", "4,466", "4,57"],
        26,
    );
    read(
        &path,
        "SELECT a.g + 1 AS k, b.v FROM s.a a JOIN s.b b ON b.a_id = a.id \
         WHERE a.g = 3 AND b.v < 500 ORDER BY k, 2",
        &["4,135", "4,310", "4,388", "4,466", "4,57"],
        24,
    );
    same(
        &path,
        "SELECT a.id AS k, count(*) FROM s.a a JOIN s.b b ON b.a_id = a.id \
         GROUP BY k ORDER BY k LIMIT 2",
        &["0,6", "1,6"],
    );
    same(
        &path,
        "SELECT a.id FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 3 ORDER BY id LIMIT 2",
        &["3", "3"],
    );
    same(&path, "SELECT g FROM s.a ORDER BY id LIMIT 2", &["0", "1"]);
}

#[test]
fn a_window_reads_what_it_partitions_and_orders_by() {
    let path = snail("window");
    let q = "FROM s.a a JOIN s.b b ON b.a_id = a.id WHERE a.g = 3 AND b.v < 500";
    let ones = [
        "name 1458,1",
        "name 2137,1",
        "name 2719,1",
        "name 294,1",
        "name 876,1",
    ];
    same(
        &path,
        &format!("SELECT a.name, row_number() OVER (PARTITION BY b.id ORDER BY b.v) {q}"),
        &ones,
    );
    same(
        &path,
        &format!("SELECT a.name, count(*) OVER w {q} WINDOW w AS (PARTITION BY b.id)"),
        &ones,
    );
    same(
        &path,
        &format!("SELECT a.name, count(*) OVER (PARTITION BY a.id) {q}"),
        &ones,
    );
    same(
        &path,
        &format!(
            "SELECT a.name, sum(b.v) OVER (ORDER BY b.id ROWS BETWEEN 1 PRECEDING AND CURRENT ROW) {q}"
        ),
        &[
            "name 1458,466",
            "name 2137,192",
            "name 2719,445",
            "name 294,698",
            "name 876,854",
        ],
    );
    // A partition by `a.g` leaves `b.id` unread.
    read(
        &path,
        &format!("SELECT a.name, count(*) OVER (PARTITION BY a.g) {q}"),
        &[
            "name 1458,5",
            "name 2137,5",
            "name 2719,5",
            "name 294,5",
            "name 876,5",
        ],
        36,
    );
}

#[test]
fn a_column_named_as_an_alias_is_read_as_a_column() {
    let path = snail("as_alias");
    // Alone, it is the column before it is the relation.
    for q in [
        "SELECT a FROM s.c a WHERE a.id < 2",
        "SELECT a.a FROM s.c a WHERE a.id < 2",
    ] {
        same(&path, q, &["0", "7"]);
    }
    same(&path, "SELECT count(a) FROM s.c a", &["1000"]);
    // `s.c`'s column `a`, and the relation `a`: `a.id` is no name of the column's.
    same(
        &path,
        "SELECT x.a + 1 FROM s.c x JOIN s.a a ON a.id = x.a WHERE a.g = 3 AND x.id < 100",
        &["295"],
    );
    read(
        &path,
        "SELECT count(x.id + 1) FROM s.c x JOIN s.a AS a ON a.id = x.id",
        &["1000"],
        7,
    );
    // Written with no AS, an alias counts as a name by itself: a column too many, and the same answer.
    same(
        &path,
        "SELECT count(x.id + 1) FROM s.c x JOIN s.a a ON a.id = x.id",
        &["1000"],
    );
}

#[test]
fn a_relation_nothing_names_gives_its_rows_all_the_same() {
    let path = snail("unnamed");
    for (q, want) in [
        ("SELECT count(a.id + 1) FROM s.a a, s.d x WHERE a.id < 1", "3"),
        ("SELECT count(a.id + 1) FROM s.a a CROSS JOIN s.d x WHERE a.id < 1", "3"),
        ("SELECT count(a.id + 1) FROM s.a a LEFT JOIN s.d x ON a.g = 1000 WHERE a.id < 1", "1"),
        ("SELECT count(a.id + 1) FROM s.a a LEFT JOIN s.d x ON a.g = 0 WHERE a.id < 1", "3"),
        ("SELECT count(a.id + 1) FROM s.d x RIGHT JOIN s.a a ON a.g = 0 WHERE a.id < 1", "3"),
        ("SELECT count(x.id + 1) FROM s.a a, s.d x WHERE a.id < 1", "3"),
        ("SELECT count(a.id + 1) FROM s.a a, s.d x, s.d y WHERE a.id < 1", "9"),
        ("SELECT count(y.id + 1) FROM s.a a, s.d x, s.d y WHERE a.id < 1 AND x.id < 2 AND y.w < 12", "4"),
    ] {
        same(&path, q, &[want]);
    }
}
