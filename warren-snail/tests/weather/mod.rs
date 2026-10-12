//! Weather stations, their readings and a wind log, generated from a seed and written into snails,
//! with the plain rows every answer is checked against.
#![allow(dead_code)]

use arrow::array::{
    Array, Int16Array, Int32Array, Int64Array, RecordBatch, StringArray, TimestampMicrosecondArray,
};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use warren_snail::catalog::Catalog;
use warren_snail::read::{instant, Round, Snail};
use warren_snail::values::Values;
use warren_snail::write::{Rows, Written};

/// The stations and their readings.
pub const DDL: &str = include_str!("../weather.sql");
/// Every reading the sample holds: those observed in 2021-2023, UTC.
pub const SPAN: &str =
    "r.observed_at >= '2021-01-01 00:00:00+00' AND r.observed_at < '2024-01-01 00:00:00+00'";
/// A reading's month of observation, as the month's index keys it.
pub const MONTH: &str = "extract(month FROM weather.clock(r.observed_at))::smallint";

/// The feeds a reading comes in on, the commonest first.
pub const FEEDS: [&str; 5] = [
    "AWS hourly",
    "AWS Main",
    "observer",
    "synop relay",
    "AWS Backup",
];

/// The places stations are named after: some are the start of others' names, and some are written
/// with letters past ASCII.
const PLACES: [&str; 12] = [
    "Faro",
    "Évora",
    "Lisboa",
    "Lisboa Gago Coutinho",
    "Lisboa Geofísico",
    "Lisboa–Tejo",
    "Porto",
    "Porto Santo",
    "Portoferraio",
    "Pörtschach",
    "Zürich",
    "Ålesund",
];

/// The readings, column by column.
pub struct Sample {
    pub reading_id: Vec<i64>,
    pub network_id: Vec<i64>,
    pub station_no: Vec<i64>,
    pub feed: Vec<String>,
    pub observed_at: Vec<i64>,
    pub checked_at: Vec<Option<i64>>,
}

/// `n` readings: each from one of 54,000 stations, observed in 2021-2023, and checked within ten
/// days of it, or, one in a hundred, not yet.
pub fn sample(n: usize) -> Sample {
    let mut x: u64 = 0x9e37_79b9_7f4a_7c15;
    let mut next = move || {
        x = x
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        x >> 17
    };
    let start = instant("2021-01-01 00:00:00+00").unwrap();
    let end = instant("2024-01-01 00:00:00+00").unwrap();
    let mut s = Sample {
        reading_id: vec![],
        network_id: vec![],
        station_no: vec![],
        feed: vec![],
        observed_at: vec![],
        checked_at: vec![],
    };
    for i in 0..n {
        s.reading_id
            .push(1_000_000 + (i as i64) * 7 + (next() % 5) as i64);
        s.network_id.push(1 + (next() % 9000) as i64);
        s.station_no.push(1 + (next() % 6) as i64);
        s.feed
            .push(FEEDS[[0, 0, 0, 0, 1, 1, 1, 2, 2, 3, 4][(next() % 11) as usize]].to_string());
        let at = start + (next() as i64).rem_euclid(end - start);
        s.observed_at.push(at);
        s.checked_at.push(if next() % 100 == 0 {
            None
        } else {
            Some(at + (next() % 864_000_000_000) as i64)
        });
    }
    s
}

/// A station's name: its place, then a number.
pub fn name(network_id: i64, station_no: i64) -> String {
    format!(
        "{} {}",
        PLACES[(network_id % 12) as usize],
        (network_id * 7 + station_no) % 50
    )
}

/// The sample's stations: every (network_id, station_no) a reading names, once, in order.
pub fn stations_of(s: &Sample) -> Vec<(i64, i64)> {
    let mut pairs: Vec<(i64, i64)> = s
        .network_id
        .iter()
        .copied()
        .zip(s.station_no.iter().copied())
        .collect();
    pairs.sort();
    pairs.dedup();
    pairs
}

pub fn ints(v: &[i64]) -> Values {
    Values::Int(v.iter().map(|&x| Some(x)).collect())
}

pub fn stations(s: &Sample) -> Rows {
    let pairs = stations_of(s);
    Rows {
        columns: vec![
            Values::Int(pairs.iter().map(|p| Some(p.0)).collect()),
            Values::Int(pairs.iter().map(|p| Some(p.1)).collect()),
            Values::Text(pairs.iter().map(|p| Some(name(p.0, p.1))).collect()),
        ],
    }
}

pub fn readings(s: &Sample) -> Rows {
    Rows {
        columns: vec![
            ints(&s.reading_id),
            ints(&s.network_id),
            ints(&s.station_no),
            Values::Text(s.feed.iter().map(|v| Some(v.clone())).collect()),
            ints(&s.observed_at),
            Values::Int(s.checked_at.clone()),
        ],
    }
}

/// Both tables of the sample, as `write` takes them.
pub fn tables(s: &Sample) -> Vec<(String, Rows)> {
    vec![
        ("weather.stations".into(), stations(s)),
        ("weather.readings".into(), readings(s)),
    ]
}

/// The pages each of the tests' snails holds in a file: `SNAIL_SPAN`, or one file's worth.
pub fn span() -> u64 {
    std::env::var("SNAIL_SPAN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(warren_snail::file::FILE_PAGES)
}

/// A path for a test's file, nothing left at it.
pub fn tmp(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&path);
    path
}

/// A new snail, its pages in files of the tests' span.
pub fn write(
    path: &Path,
    catalog: &Catalog,
    tables: &[(String, Rows)],
) -> warren_snail::Result<Vec<Written>> {
    warren_snail::write::write_spanned(path, catalog, tables, &[], span())
}

/// The sample's snail.
pub fn snail(name: &str, s: &Sample) -> PathBuf {
    snail_spanned(name, s, span())
}

/// The sample's snail, its pages in files of `span` pages each.
pub fn snail_spanned(name: &str, s: &Sample, span: u64) -> PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(DDL).unwrap();
    warren_snail::write::write_spanned(&path, &catalog, &tables(s), &[], span).unwrap();
    path
}

/// A snail holding no pages between rounds, so that every round is priced as a fresh snail prices it.
pub fn fresh(path: &Path) -> Snail {
    #[cfg(feature = "standalone")]
    return Snail::open_holding(path, 0).unwrap();
    #[cfg(not(feature = "standalone"))]
    Snail::open(path).unwrap()
}

/// The bytes of a snail's files on the disk.
pub fn snail_bytes(path: &Path) -> u64 {
    let mut total = std::fs::metadata(path).unwrap().len();
    for n in 1.. {
        let mut p = path.as_os_str().to_owned();
        p.push(format!(".{n}"));
        match std::fs::metadata(PathBuf::from(p)) {
            Ok(m) => total += m.len(),
            Err(_) => break,
        }
    }
    total
}

/// A round read exactly the column pages it predicted, less those it held, and predicted them within
/// its order's bounds.
pub fn exact(q: &str, round: &Round) {
    assert_eq!(
        round.column_pages_read,
        round.column_pages_predicted - round.column_pages_held,
        "{q}: {round:?}"
    );
    assert!(
        round.column_pages_low <= round.column_pages_predicted
            && round.column_pages_predicted <= round.column_pages_high,
        "{q}: {round:?}"
    );
}

/// The station with the most readings, the first of them in order where several tie.
pub fn busiest(s: &Sample) -> (i64, i64) {
    let mut tally: HashMap<(i64, i64), usize> = Default::default();
    for i in 0..s.network_id.len() {
        *tally.entry((s.network_id[i], s.station_no[i])).or_default() += 1;
    }
    *tally
        .iter()
        .max_by_key(|(k, n)| (**n, std::cmp::Reverse(**k)))
        .unwrap()
        .0
}

/// The name the most stations share, the first of them in order where several tie.
pub fn commonest_name(s: &Sample) -> String {
    let mut names: HashMap<String, usize> = Default::default();
    for p in stations_of(s) {
        *names.entry(name(p.0, p.1)).or_default() += 1;
    }
    names
        .into_iter()
        .max_by_key(|(k, n)| (*n, std::cmp::Reverse(k.clone())))
        .unwrap()
        .0
}

/// The sample's readings in the table's order: (network, station, reading), so that neighbours share
/// a block of `r_station` and the two ends lie in blocks apart.
pub fn in_order(s: &Sample) -> Vec<(i64, i64, i64)> {
    let mut v: Vec<(i64, i64, i64)> = (0..s.reading_id.len())
        .map(|i| (s.network_id[i], s.station_no[i], s.reading_id[i]))
        .collect();
    v.sort();
    v
}

pub fn timestamps(a: &dyn Array) -> Vec<Option<i64>> {
    a.as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .unwrap()
        .iter()
        .collect()
}

pub fn texts(a: &dyn Array) -> Vec<Option<String>> {
    a.as_any()
        .downcast_ref::<StringArray>()
        .unwrap()
        .iter()
        .map(|v| v.map(str::to_string))
        .collect()
}

pub fn ints64(a: &dyn Array) -> Vec<Option<i64>> {
    if let Some(x) = a.as_any().downcast_ref::<Int64Array>() {
        return x.iter().collect();
    }
    if let Some(x) = a.as_any().downcast_ref::<Int32Array>() {
        return x.iter().map(|v| v.map(i64::from)).collect();
    }
    let x = a.as_any().downcast_ref::<Int16Array>().unwrap();
    x.iter().map(|v| v.map(i64::from)).collect()
}

/// The one value of a one-row answer's first column.
pub fn one(b: &RecordBatch) -> i64 {
    assert_eq!(b.num_rows(), 1);
    ints64(b.column(0).as_ref())[0].unwrap()
}

/// Row `r` of a text or integer column, as text.
pub fn texts_or_ints(a: &dyn Array, r: usize) -> Option<String> {
    if let Some(t) = a.as_any().downcast_ref::<StringArray>() {
        return t.is_valid(r).then(|| t.value(r).to_string());
    }
    if let Some(x) = a.as_any().downcast_ref::<Int16Array>() {
        return x.is_valid(r).then(|| x.value(r).to_string());
    }
    ints64(a)[r].map(|v| v.to_string())
}

/// One cell of an answer as text, NULL as none.
pub fn cell(a: &dyn Array, r: usize) -> Option<String> {
    if let Some(t) = a.as_any().downcast_ref::<TimestampMicrosecondArray>() {
        return t.is_valid(r).then(|| t.value(r).to_string());
    }
    texts_or_ints(a, r)
}

/// An answer's rows, each cell as text, sorted.
pub fn bag(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    let mut rows: Vec<Vec<Option<String>>> = (0..b.num_rows())
        .map(|r| b.columns().iter().map(|a| cell(a.as_ref(), r)).collect())
        .collect();
    rows.sort();
    rows
}

/// An answer of (text, count) rows, in its order.
pub fn counted(b: &RecordBatch) -> Vec<(String, i64)> {
    texts(b.column(0).as_ref())
        .into_iter()
        .map(Option::unwrap)
        .zip(ints64(b.column(1).as_ref()).into_iter().map(Option::unwrap))
        .collect()
}

/// A block's place or key as the directory lists it, as numbers.
pub fn numbers(v: &str) -> Vec<i64> {
    v.trim_matches(|c| c == '[' || c == ']')
        .split(", ")
        .filter(|x| !x.is_empty())
        .map(|x| x.parse().unwrap())
        .collect()
}

/// A reading's feed, read by its id.
pub fn feed_of(snail: &mut Snail, id: i64) -> String {
    let (b, _) = snail
        .ask(
            &format!("SELECT r.feed FROM weather.readings r WHERE r.reading_id = {id}"),
            None,
        )
        .unwrap();
    texts(b.column(0).as_ref())[0].clone().unwrap()
}

/// The first key of an order's block, as two numbers.
pub fn blocks_first(s: &Snail, order: &str, block: u64) -> (i64, i64) {
    let b = s
        .blocks()
        .unwrap()
        .into_iter()
        .find(|x| x.order == order && x.block == block)
        .unwrap();
    let v = numbers(&b.first);
    (v[0], v[1])
}

/// The readings and the stations as plain rows, changed as the statements change them.
pub struct Model {
    pub readings: Vec<(i64, i64, i64, String, i64, Option<i64>)>,
    pub stations: Vec<(i64, i64, String)>,
}

impl Model {
    pub fn of(s: &Sample) -> Model {
        Model {
            readings: (0..s.reading_id.len())
                .map(|i| {
                    (
                        s.reading_id[i],
                        s.network_id[i],
                        s.station_no[i],
                        s.feed[i].clone(),
                        s.observed_at[i],
                        s.checked_at[i],
                    )
                })
                .collect(),
            stations: stations_of(s)
                .into_iter()
                .map(|(n, st)| (n, st, name(n, st)))
                .collect(),
        }
    }

    /// The rows written afresh into a snail of their own.
    pub fn written(&self, name: &str) -> PathBuf {
        let path = tmp(&format!("{name}.snail"));
        let r = &self.readings;
        let s = &self.stations;
        let readings = Rows {
            columns: vec![
                Values::Int(r.iter().map(|x| Some(x.0)).collect()),
                Values::Int(r.iter().map(|x| Some(x.1)).collect()),
                Values::Int(r.iter().map(|x| Some(x.2)).collect()),
                Values::Text(r.iter().map(|x| Some(x.3.clone())).collect()),
                Values::Int(r.iter().map(|x| Some(x.4)).collect()),
                Values::Int(r.iter().map(|x| x.5).collect()),
            ],
        };
        let stations = Rows {
            columns: vec![
                Values::Int(s.iter().map(|x| Some(x.0)).collect()),
                Values::Int(s.iter().map(|x| Some(x.1)).collect()),
                Values::Text(s.iter().map(|x| Some(x.2.clone())).collect()),
            ],
        };
        write(
            &path,
            &Catalog::parse(DDL).unwrap(),
            &[
                ("weather.stations".into(), stations),
                ("weather.readings".into(), readings),
            ],
        )
        .unwrap();
        path
    }
}

/// Each question asked of both snails, along every order that serves it: the same rows, and every
/// read of the first its predicted pages.
pub fn same_answers(written: &mut Snail, afresh: &mut Snail, questions: &[String]) {
    for q in questions {
        let (want, first) = afresh.ask(q, None).unwrap();
        let orders: Vec<Option<String>> = if first.served.is_empty() {
            vec![None]
        } else {
            first.served.iter().cloned().map(Some).collect()
        };
        for o in orders {
            let (got, round) = written.ask(q, o.as_deref()).unwrap();
            assert_eq!(bag(&got), bag(&want), "{q} along {o:?}");
            assert_eq!(
                round.column_pages_read,
                round.column_pages_predicted - round.column_pages_held,
                "{q} along {o:?}: {round:?}"
            );
        }
    }
}

/// Every order's blocks as the directory lists them, apart from where their pages lie.
pub type Layout = Vec<(String, String, u64, u64, String, u64, u64, u64)>;

pub fn layout(s: &Snail) -> Layout {
    s.blocks()
        .unwrap()
        .into_iter()
        .map(|b| {
            (
                b.table,
                b.order,
                b.block,
                b.rows,
                b.column,
                b.dictionary_pages,
                b.data_pages,
                b.disk_pages,
            )
        })
        .collect()
}

/// A Parquet file read back by the parquet library: its batches, and its row groups.
pub fn parquet_of(path: &Path) -> (Vec<RecordBatch>, usize) {
    let b = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        std::fs::File::open(path).unwrap(),
    )
    .unwrap();
    let groups = b.metadata().num_row_groups();
    let batches = b.build().unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    (batches, groups)
}

/// The wind log: a line's place in the log, its station's code as bytes, and the turn in tenths of
/// a degree.
pub const WIND_DDL: &str = "CREATE TABLE weather.wind (seq bigint NOT NULL, station bytea NOT NULL, turn numeric(5,1) NOT NULL, PRIMARY KEY (seq));";
/// The wind, run by run.
pub const WIND: &str = include_str!("../wind.sql");

/// The wind's parameters: 24 lines a day, steady within 180 degrees, a circle of 360.
pub fn wind_params() -> Vec<String> {
    ["24", "180.0", "360.0"]
        .iter()
        .map(|s| s.to_string())
        .collect()
}

/// `n` lines of the wind log, (seq, station, turn in tenths), in runs from one station each, the runs
/// made to end each of the ways the wind's statement tells apart.
pub fn winds(n: usize) -> Vec<(i64, Vec<u8>, i64)> {
    const STATIONS: [&[u8]; 6] = [
        b"\x08\x53\x5f",
        b"\xf0\xf1",
        b"\x40\x40\x08",
        b"\x08\x53",
        b"\xff",
        b"\x08\x53\x5f\x00",
    ];
    let mut x: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let mut out = Vec::with_capacity(n);
    let mut seq = 1000i64;
    while out.len() < n {
        let station = STATIONS[(next() % STATIONS.len() as u64) as usize].to_vec();
        let len = 1 + (next() % 12) as usize;
        let kind = next() % 4;
        for _ in 0..len {
            let small = (next() % 600) as i64 - 300;
            let large = 900 + (next() % 800) as i64;
            let turn = match kind {
                0 => small,
                1 => large,
                2 => -large,
                _ => {
                    if next() % 2 == 0 {
                        large
                    } else {
                        -large
                    }
                }
            };
            out.push((seq, station.clone(), turn));
            seq += 1 + (next() % 3) as i64;
        }
    }
    out.truncate(n);
    out
}

/// The wind read plainly, as its statement spells it: (run, station, hours, wind, turned, first seq,
/// last seq), a turn as PostgreSQL writes a numeric of scale 1, the bounds in tenths.
pub fn turned(
    rows: &[(i64, Vec<u8>, i64)],
    per_day: i64,
    steady: i64,
    circle: i64,
) -> Vec<Vec<Option<String>>> {
    let text = |t: i64| warren_snail::read::numeric_text(t as i128, 1);
    let mut out = Vec::new();
    let mut i = 0;
    let mut run = 0;
    while i < rows.len() {
        let mut j = i;
        while j < rows.len() && rows[j].1 == rows[i].1 {
            j += 1;
        }
        run += 1;
        let r = &rows[i..j];
        let mut so_far = 0i64;
        let (mut reach, mut came_round) = (0i64, false);
        for x in r {
            so_far += x.2;
            reach = reach.max(so_far.abs());
            came_round |= so_far < 0 && so_far % circle == 0;
        }
        let net: i64 = r.iter().map(|x| x.2).sum();
        let veering = r.iter().all(|x| x.2 >= 0);
        let backing = r.iter().all(|x| x.2 <= 0);
        let (wind, turned) = if reach <= steady {
            ("steady", Some(text(net)))
        } else if veering {
            ("veered", Some(text(net % circle)))
        } else if backing && !came_round {
            ("backed", Some(text(-((-net) % circle))))
        } else {
            ("shifting", None)
        };
        let mut hex = String::from("\\x");
        for b in &r[0].1 {
            hex.push_str(&format!("{b:02x}"));
        }
        out.push(vec![
            Some(run.to_string()),
            Some(hex),
            Some((r.len() as i64 % per_day).to_string()),
            Some(wind.to_string()),
            turned,
            Some(r[0].0.to_string()),
            Some(r[r.len() - 1].0.to_string()),
        ]);
        i = j;
    }
    out
}

/// The wind read plainly with the statement's own parameters.
pub fn turned_as_asked(rows: &[(i64, Vec<u8>, i64)]) -> Vec<Vec<Option<String>>> {
    turned(rows, 24, 1800, 3600)
}

/// An answer's rows as text, PostgreSQL's way: numerics at their scale, bytes in hex.
pub fn as_written(b: &RecordBatch) -> Vec<Vec<Option<String>>> {
    let rel = warren_snail::read::pipeline_rows(b).unwrap();
    rel.into_iter()
        .map(|r| r.into_iter().map(|v| v.text()).collect())
        .collect()
}

/// The wind log's lines as `write` takes them.
pub fn wind_rows(rows: &[(i64, Vec<u8>, i64)]) -> Rows {
    Rows {
        columns: vec![
            Values::Int(rows.iter().map(|r| Some(r.0)).collect()),
            Values::Utf8(
                rows.iter()
                    .map(|r| Some(parquet::data_type::ByteArray::from(r.1.clone())))
                    .collect(),
            ),
            Values::Int(rows.iter().map(|r| Some(r.2)).collect()),
        ],
    }
}

/// A snail holding the wind log alone, written afresh.
pub fn wind_snail(name: &str, rows: &[(i64, Vec<u8>, i64)]) -> PathBuf {
    let path = tmp(&format!("{name}.snail"));
    let catalog = Catalog::parse(WIND_DDL).unwrap();
    write(&path, &catalog, &[("weather.wind".into(), wind_rows(rows))]).unwrap();
    path
}

/// The blocks a snail cuts the wind log into.
pub fn wind_blocks(s: &Snail) -> usize {
    s.blocks()
        .unwrap()
        .iter()
        .filter(|b| b.table == "weather.wind")
        .map(|b| b.block)
        .collect::<std::collections::BTreeSet<_>>()
        .len()
}

/// Counts of the readings each of `keep` names, by `key`, in the key's order.
pub fn tally(
    s: &Sample,
    keep: &dyn Fn(usize) -> bool,
    key: &dyn Fn(usize) -> String,
) -> Vec<(String, i64)> {
    let mut m: BTreeMap<String, i64> = Default::default();
    for i in (0..s.feed.len()).filter(|&i| keep(i)) {
        *m.entry(key(i)).or_default() += 1;
    }
    m.into_iter().collect()
}
