//! The places of an order's levels, outermost first: the sublevels its blocks are cut on.
//!
//! An integer is cut by its bits, the top one first. An instant is cut on the UTC clock by its year,
//! then its month, day, hour, minute, second and microsecond. A month is cut by the year's halves,
//! then the month. Text is cut by its bytes, the first one first, a value that has ended coming
//! before any byte.

use crate::catalog::{ColumnType, Level, Order, Table};
use chrono::{DateTime, Datelike, Timelike};
use std::fmt;

pub fn civil_utc(micros: i64) -> DateTime<chrono::Utc> {
    DateTime::from_timestamp_micros(micros).expect("an instant PostgreSQL holds")
}

/// The month of an instant on the UTC clock, from its days since 1970-01-01 on the proleptic Gregorian
/// calendar.
pub fn month_utc(micros: i64) -> i64 {
    let z = micros.div_euclid(86_400_000_000) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    if mp < 10 {
        mp + 3
    } else {
        mp - 9
    }
}

/// The month of an instant as PostgreSQL extracts it: none for an infinite one.
pub fn month_of(micros: i64) -> Option<i64> {
    (micros != i64::MAX && micros != i64::MIN).then(|| month_utc(micros))
}

/// One level's value in an order's key: a number (an integer, an instant's microseconds, a month),
/// or text, compared by its bytes. Every value of one level is of one kind.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum KeyValue {
    Int(i64),
    Text(Box<str>),
}

impl KeyValue {
    /// The number a numeric level keys.
    pub fn int(&self) -> i64 {
        match self {
            KeyValue::Int(v) => *v,
            KeyValue::Text(_) => panic!("a numeric level holds text"),
        }
    }
}

impl fmt::Display for KeyValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            KeyValue::Int(v) => write!(f, "{v}"),
            KeyValue::Text(s) => write!(f, "'{}'", s.replace('\'', "''")),
        }
    }
}

/// A key as text: its values, outermost first.
pub fn shown(key: &[KeyValue]) -> String {
    format!(
        "[{}]",
        key.iter()
            .map(|v| v.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The value a numeric level keys, from its column's value.
pub fn key(level: Level, value: i64) -> KeyValue {
    match level {
        Level::Column(_) => KeyValue::Int(value),
        Level::MonthUtc(_) => KeyValue::Int(month_utc(value)),
    }
}

/// Whether a level is read by equality, rather than walked as a line.
pub fn by_equality(level: Level, table: &Table) -> bool {
    match level {
        Level::MonthUtc(_) => true,
        Level::Column(c) => table.columns[c].ty != ColumnType::Timestamptz,
    }
}

fn sublevel_count(level: Level, ty: ColumnType, text_sublevels: u32) -> usize {
    match level {
        Level::MonthUtc(_) => 2,
        Level::Column(_) => match ty {
            ColumnType::Int2 => 16,
            ColumnType::Int4 => 32,
            ColumnType::Int8 | ColumnType::Numeric { .. } => 64,
            ColumnType::Timestamptz => 7,
            ColumnType::Text(_) | ColumnType::Bytes => text_sublevels as usize,
        },
    }
}

fn sublevel_of(level: Level, ty: ColumnType, key: &KeyValue, r: usize) -> i64 {
    if let KeyValue::Text(s) = key {
        return s.as_bytes().get(r).map_or(-1, |&b| b as i64);
    }
    let key = key.int();
    match level {
        Level::MonthUtc(_) => match r {
            0 => (key - 1) / 6,
            _ => key,
        },
        Level::Column(_) => match ty {
            ColumnType::Int2 | ColumnType::Int4 | ColumnType::Int8 | ColumnType::Numeric { .. } => {
                let bits = ty.width().unwrap() * 8;
                let low = -(1i128 << (bits - 1));
                let biased = (key as i128 - low) as u64;
                ((biased >> (bits - 1 - r)) & 1) as i64
            }
            ColumnType::Timestamptz => {
                let t = civil_utc(key);
                match r {
                    0 => t.year() as i64,
                    1 => t.month() as i64,
                    2 => t.day() as i64,
                    3 => t.hour() as i64,
                    4 => t.minute() as i64,
                    5 => t.second() as i64,
                    _ => key.rem_euclid(1_000_000),
                }
            }
            ColumnType::Text(_) | ColumnType::Bytes => unreachable!("a text level keys text"),
        },
    }
}

/// The sublevels an order's blocks are cut on, outermost first, each a sublevel of one of its levels.
pub struct Sublevels {
    map: Vec<(usize, usize)>,
    levels: Vec<(Level, ColumnType)>,
}

impl Sublevels {
    /// `text_sublevels` holds, for each level, the sublevels a text level is cut on (its longest value's bytes
    /// and one more), and 0 for a numeric level, whose sublevels its type gives.
    pub fn new(order: &Order, table: &Table, text_sublevels: &[u32]) -> Sublevels {
        let levels: Vec<(Level, ColumnType)> = order
            .levels
            .iter()
            .map(|&l| (l, table.columns[l.column()].ty))
            .collect();
        let mut map = Vec::new();
        for (i, &(l, ty)) in levels.iter().enumerate() {
            for r in 0..sublevel_count(l, ty, text_sublevels.get(i).copied().unwrap_or(0)) {
                map.push((i, r));
            }
        }
        Sublevels { map, levels }
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// The value at sublevel `r` for a row whose levels key `key`.
    pub fn value(&self, key: &[KeyValue], r: usize) -> i64 {
        let (i, sub) = self.map[r];
        let (l, ty) = self.levels[i];
        sublevel_of(l, ty, &key[i], sub)
    }
}

/// Rows' keys, one per level of an order, laid flat.
pub struct Keys {
    pub stride: usize,
    pub data: Vec<KeyValue>,
}

impl Keys {
    pub fn row(&self, i: usize) -> &[KeyValue] {
        &self.data[i * self.stride..(i + 1) * self.stride]
    }

    pub fn len(&self) -> usize {
        self.data.len().checked_div(self.stride).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// Keys of one numeric level each, from the numbers given.
pub fn ints(values: impl IntoIterator<Item = i64>) -> Vec<KeyValue> {
    values.into_iter().map(KeyValue::Int).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    #[test]
    fn an_integer_is_cut_by_its_bits_top_first_and_keeps_its_order() {
        let c = Catalog::parse("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        assert_eq!(sublevels.len(), 32);
        let bits = |v: i64| {
            (0..32)
                .map(|r| sublevels.value(&[KeyValue::Int(v)], r))
                .collect::<Vec<_>>()
        };
        assert_eq!(bits(-1)[..2], [0, 1]);
        assert_eq!(bits(0)[..2], [1, 0]);
        assert_eq!(bits(5)[29..], [1, 0, 1]);
        let mut all: Vec<Vec<i64>> = [-7, -1, 0, 3, 1 << 20].iter().map(|&v| bits(v)).collect();
        let sorted = all.clone();
        all.sort();
        assert_eq!(all, sorted);
    }

    #[test]
    fn an_instant_is_cut_by_its_calendar_on_the_utc_clock() {
        let c = Catalog::parse(
            "CREATE FUNCTION s.c(timestamptz) RETURNS timestamp LANGUAGE sql AS $$ SELECT $1 AT TIME ZONE 'UTC' $$;
             CREATE TABLE s.t (at timestamptz NOT NULL);
             CREATE INDEX m ON s.t ((extract(month FROM s.c(at))::smallint), at);
             ALTER TABLE s.t CLUSTER ON m;",
        )
        .unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0, 0]);
        assert_eq!(sublevels.len(), 9);
        // 2017-12-31 23:59:59.000001 UTC
        let at = 1_514_764_799_000_001;
        let key = [KeyValue::Int(month_utc(at)), KeyValue::Int(at)];
        let v: Vec<i64> = (0..9).map(|r| sublevels.value(&key, r)).collect();
        assert_eq!(v, [1, 12, 2017, 12, 31, 23, 59, 59, 1]);
        assert_eq!(month_utc(at + 999_999), 1);
    }

    #[test]
    fn the_month_is_the_calendar_s_on_the_utc_clock_and_none_for_an_infinite_instant() {
        let day = 86_400_000_000i64;
        // Every day from 1600 to 2400, at its first and last microsecond.
        let (first, last) = (-11_676_096_000_000_000i64, 13_569_465_600_000_000i64);
        let mut at = first;
        while at < last {
            for t in [at, at + day - 1] {
                assert_eq!(month_utc(t), civil_utc(t).month() as i64, "{t}");
            }
            at += day;
        }
        assert_eq!(
            (month_of(i64::MAX), month_of(i64::MIN), month_of(0)),
            (None, None, Some(1))
        );
    }

    #[test]
    fn text_is_cut_by_its_bytes_and_an_ended_value_comes_first() {
        let c = Catalog::parse("CREATE TABLE s.t (s text NOT NULL, PRIMARY KEY (s));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[4]);
        assert_eq!(sublevels.len(), 4);
        let bytes = |s: &str| {
            (0..4)
                .map(|r| sublevels.value(&[KeyValue::Text(s.into())], r))
                .collect::<Vec<_>>()
        };
        assert_eq!(bytes("ab"), [97, 98, -1, -1]);
        let mut all: Vec<(Vec<i64>, &str)> = ["b", "ab", "abc", "", "a"]
            .iter()
            .map(|s| (bytes(s), *s))
            .collect();
        all.sort();
        assert_eq!(
            all.iter().map(|x| x.1).collect::<Vec<_>>(),
            ["", "a", "ab", "abc", "b"]
        );
        let mut keys = vec![
            KeyValue::Text("b".into()),
            KeyValue::Text("ab".into()),
            KeyValue::Text("".into()),
        ];
        keys.sort();
        assert_eq!(shown(&keys), "['', 'ab', 'b']");
    }
}
