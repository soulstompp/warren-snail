//! What a BRIN index keeps. It is no strand: the block is its run of pages, and each block of the
//! table's order, and each entry above the blocks, sums a key as its operator class does. A minmax is
//! the least and greatest every entry keeps of a leaf; a range's or an inet's inclusion its span; a
//! box's inclusion its leaves' least and greatest. A minmax-multi's values and runs and a bloom's
//! filter are kept here, as `Sum`.

use crate::catalog::{Level, Order, Table};
use crate::file::{In, Out};
use crate::values::Values;
use crate::{Error, Result};

/// What an operator class sums of its key's values.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Minmax,
    Inclusion,
    /// Its `values_per_range`.
    Multi(u16),
    /// Its `n_distinct_per_range` and `false_positive_rate`.
    Bloom {
        distinct: f64,
        rate: f64,
    },
}

/// PostgreSQL's defaults (`brin_minmax_multi.c`, `brin_bloom.c`).
const VALUES_PER_RANGE: u16 = 32;
const DISTINCT_PER_RANGE: f64 = -0.1;
const FALSE_POSITIVE_RATE: f64 = 0.01;
const LEAST_DISTINCT: f64 = 16.0;
const SEED_1: u64 = 0x71d9_24af;
const SEED_2: u64 = 0xba48_b314;

/// The most bytes a run keeps of a value keyed by its bytes.
const RUN_BOUND: usize = 64;
/// The most values a bloom's test walks of a condition that keeps a few.
pub(crate) const WALKED: i128 = 64;

/// An operator class's kind, its parameters as written.
pub(crate) fn kind_of(opclass: &str, params: Option<&str>) -> Kind {
    let read = |name: &str| params.and_then(|p| crate::methods::opclass_param(p, name));
    if opclass.ends_with("_minmax_multi_ops") {
        Kind::Multi(
            read("values_per_range")
                .and_then(|v| v.parse().ok())
                .unwrap_or(VALUES_PER_RANGE),
        )
    } else if opclass.ends_with("_bloom_ops") {
        let real = |name: &str, default: f64| {
            read(name)
                .and_then(|v| v.parse::<f64>().ok())
                .unwrap_or(default)
        };
        Kind::Bloom {
            distinct: real("n_distinct_per_range", DISTINCT_PER_RANGE),
            rate: real("false_positive_rate", FALSE_POSITIVE_RATE),
        }
    } else if opclass.ends_with("_inclusion_ops") {
        Kind::Inclusion
    } else {
        Kind::Minmax
    }
}

/// The keys whose `Sum` each entry of an order keeps: on the table's order, each BRIN key that needs
/// more than an entry keeps already, once each. A minmax on a column's month keeps its least and
/// greatest as a run.
pub(crate) fn kept(table: &Table, order: &Order) -> Vec<(Level, Kind)> {
    let mut out = Vec::new();
    if !order.table_order {
        return out;
    }
    for b in &table.brins {
        for (i, &l) in b.levels.iter().enumerate() {
            if order.slot(l.column()).is_none() {
                continue;
            }
            let kind = match kind_of(&b.opclasses[i], b.params[i].as_deref()) {
                Kind::Minmax if matches!(l, Level::MonthUtc(_)) => Kind::Multi(2),
                Kind::Minmax | Kind::Inclusion => continue,
                kind => kind,
            };
            if !out.contains(&(l, kind)) {
                out.push((l, kind));
            }
        }
    }
    out
}

/// How many summaries each entry of an order keeps.
pub fn kept_count(table: &Table, order: &Order) -> usize {
    kept(table, order).len()
}

/// The slots of the first leaves of the columns whose `Sum` an order's entries keep.
pub(crate) fn slots(table: &Table, order: &Order) -> Vec<u32> {
    kept(table, order)
        .into_iter()
        .filter_map(|(l, _)| order.leaf_slot(table, l.column()))
        .map(|s| s as u32)
        .collect()
}

/// The columns of a table's order whose span a BRIN inclusion keeps: a range's, an inet's.
pub(crate) fn spanned(table: &Table, order: &Order) -> Vec<usize> {
    let mut out = Vec::new();
    if !order.table_order {
        return out;
    }
    for b in &table.brins {
        for (i, &l) in b.levels.iter().enumerate() {
            let c = l.column();
            let ty = table.columns[c].ty;
            if kind_of(&b.opclasses[i], b.params[i].as_deref()) == Kind::Inclusion
                && (crate::spans::is_inet(ty) || crate::spans::is_period(ty))
                && !out.contains(&c)
            {
                out.push(c);
            }
        }
    }
    out
}

/// A number's key as bytes that sort as it does.
pub fn number_key(v: i128) -> [u8; 16] {
    ((v as u128) ^ (1 << 127)).to_be_bytes()
}

/// The number `number_key` keyed.
pub fn number_of(key: &[u8]) -> Option<i128> {
    let b: [u8; 16] = key.try_into().ok()?;
    Some((u128::from_be_bytes(b) ^ (1 << 127)) as i128)
}

/// The key row `r` of a column is summed by, where it holds a value: what a condition on the key
/// compares.
fn key_at(table: &Table, level: Level, values: &Values, r: usize) -> Result<Option<Vec<u8>>> {
    let c = level.column();
    let column = &table.columns[c];
    Ok(match level {
        Level::MonthUtc(_) => values
            .int(r)
            .and_then(crate::places::month_of)
            .map(|m| number_key(m.into()).to_vec()),
        Level::Scaled(_, k) => values
            .int(r)
            .map(|x| number_key(crate::places::scaled_key(x, k).into()).to_vec()),
        Level::Column(_) if crate::write::bounded(table, c) => {
            crate::write::compared_key(column.ty, column.orderer()?, values, r)
        }
        Level::Column(_) => values.wide(r).map(|v| number_key(v).to_vec()),
    })
}

/// Each `Sum` an order's entry keeps, of the rows given.
pub(crate) fn summed(
    table: &Table,
    order: &Order,
    columns: &[Values],
    rows: &[usize],
) -> Result<Vec<Sum>> {
    kept(table, order)
        .into_iter()
        .map(|(l, kind)| {
            let mut keys = Vec::with_capacity(rows.len());
            for &r in rows {
                keys.extend(key_at(table, l, &columns[l.column()], r)?);
            }
            Ok(Sum::of(kind, keys))
        })
        .collect()
}

/// A `Sum` widened to hold a row added to a block of `rows` rows, its column's value as it is written.
pub(crate) fn add_row(
    sum: &mut Sum,
    table: &Table,
    level: Level,
    kind: Kind,
    value: &crate::read::statement::Datum,
    rows: u32,
) -> Result<()> {
    let mut one = Values::new(table.columns[level.column()].ty);
    crate::values::push_datum(&mut one, value);
    if let Some(key) = key_at(table, level, &one, 0)? {
        sum.add(kind, key, rows as usize);
    }
    Ok(())
}

/// A BRIN key's summary of the values under an entry, beyond what the entry keeps already.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Sum {
    /// It sums no value yet.
    Nothing,
    /// A bloom's filter: `bits` bits, a power of two, and the hashes a value sets.
    Filter { bits: u32, hashes: u8, set: Vec<u8> },
    /// A minmax-multi's values and runs, rising and apart, each its first and last key; together at
    /// most `most` of them, a value counted once.
    Runs {
        most: u16,
        runs: Vec<(Vec<u8>, Vec<u8>)>,
    },
}

/// The two hashes of a key under PostgreSQL's fixed seeds, the second odd: a value sets bit
/// `(h1 + i h2) mod bits`.
fn hashed(key: &[u8]) -> (u32, u32) {
    let one = |seed: u64| {
        let mut h = 0xcbf2_9ce4_8422_2325 ^ seed;
        for &b in key {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        h ^= h >> 33;
        h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
        h ^= h >> 33;
        h = h.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        (h ^ (h >> 33)) as u32
    };
    (one(SEED_1), one(SEED_2) | 1)
}

/// PostgreSQL's size for a filter of `values` values (`brin_bloom_get_ndistinct`,
/// `bloom_filter_size`), its bits rounded up to a power of two, and its hashes.
pub(crate) fn filter_size(distinct: f64, rate: f64, values: usize) -> (u32, u8) {
    let rows = values.max(1) as f64;
    let n = if distinct < 0.0 {
        -distinct * rows
    } else {
        distinct
    };
    let n = n.max(LEAST_DISTINCT).min(rows).trunc().max(1.0);
    let bits = (-(n * rate.ln()) / 2f64.ln().powi(2)).ceil() as u64;
    let bits = bits.div_ceil(8).max(1) * 8;
    let bits = bits.next_power_of_two().min(1 << 31) as u32;
    let hashes = (2f64.ln() * f64::from(bits) / n).round().clamp(1.0, 255.0) as u8;
    (bits, hashes)
}

/// How far apart two keys lie, by their first 16 bytes.
fn gap(a: &[u8], b: &[u8]) -> u128 {
    let first = |k: &[u8]| {
        let mut x = [0u8; 16];
        let n = k.len().min(16);
        x[..n].copy_from_slice(&k[..n]);
        u128::from_be_bytes(x)
    };
    first(b).saturating_sub(first(a))
}

/// A key as a run: itself, or past `RUN_BOUND` bytes its first bytes and the least bytes past all
/// they head.
fn run_of(key: Vec<u8>) -> (Vec<u8>, Vec<u8>) {
    if key.len() <= RUN_BOUND {
        return (key.clone(), key);
    }
    let lo = key[..RUN_BOUND].to_vec();
    let hi = match lo.iter().rposition(|&b| b < 255) {
        Some(n) => {
            let mut hi = lo[..=n].to_vec();
            hi[n] += 1;
            hi
        }
        None => key,
    };
    (lo, hi)
}

/// How many values a run counts as.
fn counted(run: &(Vec<u8>, Vec<u8>)) -> usize {
    if run.0 == run.1 {
        1
    } else {
        2
    }
}

/// Runs rising and apart, the nearest joined until they count at most `most`.
fn reduced(runs: Vec<(Vec<u8>, Vec<u8>)>, most: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut total: usize = runs.iter().map(counted).sum();
    if total <= most {
        return runs;
    }
    let mut gaps: Vec<(u128, usize)> = runs
        .windows(2)
        .enumerate()
        .map(|(i, w)| (gap(&w[0].1, &w[1].0), i))
        .collect();
    gaps.sort_unstable();
    let mut joined = vec![false; runs.len().saturating_sub(1)];
    for (_, i) in gaps {
        if total <= most {
            break;
        }
        let alone = |r: usize, beside: Option<usize>| {
            runs[r].0 == runs[r].1 && !beside.is_some_and(|g| joined.get(g) == Some(&true))
        };
        let left = alone(i, i.checked_sub(1));
        let right = alone(i + 1, Some(i + 1));
        total = total + 2 - if left { 1 } else { 2 } - if right { 1 } else { 2 };
        joined[i] = true;
    }
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    for (i, r) in runs.into_iter().enumerate() {
        if i > 0 && joined[i - 1] {
            out.last_mut().unwrap().1 = r.1;
        } else {
            out.push(r);
        }
    }
    out
}

/// Two lists of runs as one, rising and apart.
fn merged(a: &[(Vec<u8>, Vec<u8>)], b: &[(Vec<u8>, Vec<u8>)]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut all: Vec<(Vec<u8>, Vec<u8>)> = a.iter().chain(b).cloned().collect();
    all.sort_unstable();
    let mut out: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(all.len());
    for r in all {
        match out.last_mut() {
            Some(last) if r.0 <= last.1 => {
                if r.1 > last.1 {
                    last.1 = r.1;
                }
            }
            _ => out.push(r),
        }
    }
    out
}

impl Sum {
    /// A summary of the keys given.
    pub(crate) fn of(kind: Kind, mut keys: Vec<Vec<u8>>) -> Sum {
        if keys.is_empty() {
            return Sum::Nothing;
        }
        match kind {
            Kind::Bloom { distinct, rate } => {
                let (bits, hashes) = filter_size(distinct, rate, keys.len());
                let mut sum = Sum::Filter {
                    bits,
                    hashes,
                    set: vec![0; (bits as usize / 8).max(1)],
                };
                for k in &keys {
                    sum.set_key(k);
                }
                sum
            }
            Kind::Multi(most) => {
                keys.sort_unstable();
                keys.dedup();
                let runs: Vec<_> = keys.into_iter().map(run_of).collect();
                Sum::Runs {
                    most,
                    runs: reduced(merged(&runs, &[]), most as usize),
                }
            }
            Kind::Minmax | Kind::Inclusion => Sum::Nothing,
        }
    }

    fn set_key(&mut self, key: &[u8]) {
        if let Sum::Filter { bits, hashes, set } = self {
            let (h1, h2) = hashed(key);
            for i in 0..u32::from(*hashes) {
                let j = h1.wrapping_add(i.wrapping_mul(h2)) & (*bits - 1);
                set[(j / 8) as usize] |= 1 << (j % 8);
            }
        }
    }

    /// Whether a filter may hold a key.
    pub fn filter_has(&self, key: &[u8]) -> bool {
        match self {
            Sum::Filter { bits, hashes, set } => {
                let (h1, h2) = hashed(key);
                (0..u32::from(*hashes)).all(|i| {
                    let j = h1.wrapping_add(i.wrapping_mul(h2)) & (*bits - 1);
                    set[(j / 8) as usize] & (1 << (j % 8)) != 0
                })
            }
            Sum::Nothing => false,
            Sum::Runs { .. } => true,
        }
    }

    /// It widened to hold a key written into a block of `rows` rows; where it sums nothing yet, sized
    /// for them.
    pub(crate) fn add(&mut self, kind: Kind, key: Vec<u8>, rows: usize) {
        match self {
            Sum::Nothing => {
                *self = match kind {
                    Kind::Bloom { distinct, rate } => {
                        let (bits, hashes) = filter_size(distinct, rate, rows);
                        let mut sum = Sum::Filter {
                            bits,
                            hashes,
                            set: vec![0; (bits as usize / 8).max(1)],
                        };
                        sum.set_key(&key);
                        sum
                    }
                    _ => Sum::of(kind, vec![key]),
                }
            }
            Sum::Filter { .. } => self.set_key(&key),
            Sum::Runs { most, runs } => {
                let joined = merged(runs, &[run_of(key)]);
                *runs = reduced(joined, *most as usize);
            }
        }
    }

    /// It widened to hold every value another holds: filters folded to the smaller, testing with the
    /// fewer hashes, one every bit of which is set kept as one bit; runs joined.
    pub fn join(&mut self, other: &Sum) -> Result<()> {
        match (&mut *self, other) {
            (_, Sum::Nothing) => {}
            (Sum::Nothing, o) => *self = o.clone(),
            (
                Sum::Filter { bits, hashes, set },
                Sum::Filter {
                    bits: b,
                    hashes: h,
                    set: s,
                },
            ) => {
                let to = (*bits).min(*b);
                let mut out = folded(set, *bits, to);
                for (x, y) in out.iter_mut().zip(folded(s, *b, to)) {
                    *x |= y;
                }
                *hashes = (*hashes).min(*h);
                *bits = to;
                *set = out;
                if *bits > 1 && set.iter().all(|&x| x == u8::MAX) {
                    *bits = 1;
                    *set = vec![1];
                }
            }
            (Sum::Runs { most, runs }, Sum::Runs { runs: r, .. }) => {
                let joined = merged(runs, r);
                *runs = reduced(joined, *most as usize);
            }
            _ => return Err(Error::new("a BRIN summary joined to one of another kind")),
        }
        Ok(())
    }

    /// The bytes it takes in its entry.
    pub(crate) fn size(&self) -> usize {
        let mut o = Out::default();
        self.encode(&mut o);
        o.0.len()
    }

    pub(crate) fn encode(&self, o: &mut Out) {
        match self {
            Sum::Nothing => o.u8(0),
            Sum::Filter { bits, hashes, set } => {
                o.u8(1);
                o.u8(bits.trailing_zeros() as u8);
                o.u8(*hashes);
                o.0.extend_from_slice(set);
            }
            Sum::Runs { most, runs } => {
                o.u8(2);
                o.0.extend_from_slice(&most.to_le_bytes());
                o.0.extend_from_slice(&(runs.len() as u16).to_le_bytes());
                // What every bound shares from its first, written once.
                let first = runs.first().map_or(&[][..], |r| r.0.as_slice());
                let shared = runs
                    .iter()
                    .flat_map(|r| [&r.0, &r.1])
                    .map(|k| k.iter().zip(first).take_while(|(x, y)| x == y).count())
                    .min()
                    .unwrap_or(0)
                    .min(255);
                o.u8(shared as u8);
                o.0.extend_from_slice(&first[..shared]);
                for (lo, hi) in runs {
                    let point = lo == hi;
                    o.u8(u8::from(point));
                    o.u8((lo.len() - shared) as u8);
                    o.0.extend_from_slice(&lo[shared..]);
                    if !point {
                        o.u8((hi.len() - shared) as u8);
                        o.0.extend_from_slice(&hi[shared..]);
                    }
                }
            }
        }
    }

    pub(crate) fn decode(i: &mut In) -> Result<Sum> {
        Ok(match i.u8()? {
            0 => Sum::Nothing,
            1 => {
                let bits = 1u32 << i.u8()?;
                let hashes = i.u8()?;
                let set = i.take((bits as usize / 8).max(1))?.to_vec();
                Sum::Filter { bits, hashes, set }
            }
            2 => {
                let most = u16::from_le_bytes(i.take(2)?.try_into().unwrap());
                let n = u16::from_le_bytes(i.take(2)?.try_into().unwrap());
                let shared = i.u8()? as usize;
                let shared = i.take(shared)?.to_vec();
                let mut runs = Vec::with_capacity(n as usize);
                for _ in 0..n {
                    let point = i.u8()? != 0;
                    let bound = |i: &mut In| -> Result<Vec<u8>> {
                        let len = i.u8()? as usize;
                        let mut k = shared.clone();
                        k.extend_from_slice(i.take(len)?);
                        Ok(k)
                    };
                    let lo = bound(i)?;
                    let hi = if point { lo.clone() } else { bound(i)? };
                    runs.push((lo, hi));
                }
                Sum::Runs { most, runs }
            }
            other => return Err(Error::new(format!("a BRIN summary is marked {other}"))),
        })
    }
}

/// A filter's bytes folded to `to` bits, a power of two no more than its own: bit j into j mod `to`.
fn folded(set: &[u8], bits: u32, to: u32) -> Vec<u8> {
    if to == bits {
        return set.to_vec();
    }
    if to < 8 {
        return vec![u8::from(set.iter().any(|&x| x != 0))];
    }
    let mut out = vec![0u8; to as usize / 8];
    let n = out.len();
    for (i, &x) in set.iter().enumerate() {
        out[i % n] |= x;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOOM: Kind = Kind::Bloom {
        distinct: DISTINCT_PER_RANGE,
        rate: FALSE_POSITIVE_RATE,
    };

    fn key(v: i64) -> Vec<u8> {
        number_key(v.into()).to_vec()
    }

    #[test]
    fn an_operator_class_s_kind_and_parameters_are_read_from_its_name_and_what_is_written() {
        assert_eq!(kind_of("int4_minmax_ops", None), Kind::Minmax);
        assert_eq!(kind_of("inet_inclusion_ops", None), Kind::Inclusion);
        assert_eq!(kind_of("int4_minmax_multi_ops", None), Kind::Multi(32));
        assert_eq!(
            kind_of("int4_minmax_multi_ops", Some("values_per_range = 8")),
            Kind::Multi(8)
        );
        assert_eq!(kind_of("text_bloom_ops", None), BLOOM);
        assert_eq!(
            kind_of(
                "text_bloom_ops",
                Some("false_positive_rate = 0.05, n_distinct_per_range = '200'")
            ),
            Kind::Bloom {
                distinct: 200.0,
                rate: 0.05
            }
        );
    }

    #[test]
    fn a_filter_is_sized_by_postgresql_s_rule_rounded_up_to_a_power_of_two() {
        // snail.nu bloom: a braid block's 124 values, 1,189 bits, take 2,048; its largest 4,096.
        assert_eq!(filter_size(DISTINCT_PER_RANGE, 0.01, 1248), (2048, 11));
        assert_eq!(filter_size(DISTINCT_PER_RANGE, 0.01, 2146).0, 4096);
        // At least 16 values, at most the rows.
        assert_eq!(filter_size(DISTINCT_PER_RANGE, 0.01, 100).0, 256);
        assert_eq!(filter_size(DISTINCT_PER_RANGE, 0.01, 1).0, 16);
        // A number of values as it is, at most the rows; a rate as it is.
        assert_eq!(filter_size(1000.0, 0.01, 1248).0, 16384);
        assert_eq!(filter_size(100000.0, 0.01, 1248).0, 16384);
        assert_eq!(filter_size(DISTINCT_PER_RANGE, 0.25, 1248).0, 512);
        assert_eq!(filter_size(-1.0, 0.0001, 1024).0, 32768);
    }

    /// Blocks of the rows given, a level above them of `over` blocks an entry, and a level above that
    /// of `over` of those: every value tests present in its block's filter and in each folded above.
    fn walked_at_every_level(kind: Kind, block_rows: &[usize], over: usize) {
        let all: Vec<i64> = (i16::MIN as i64..=i16::MAX as i64).collect();
        let mut at = 0;
        let mut blocks: Vec<(Sum, Vec<i64>)> = Vec::new();
        let mut size = block_rows.iter().cycle();
        while at < all.len() {
            let n = *size.next().unwrap();
            let values = all[at..(at + n).min(all.len())].to_vec();
            at += n;
            blocks.push((
                Sum::of(kind, values.iter().map(|&v| key(v)).collect()),
                values,
            ));
        }
        let holds = |s: &Sum, v: i64| match s {
            Sum::Runs { runs, .. } => {
                let k = key(v);
                runs.iter().any(|(a, b)| *a <= k && k <= *b)
            }
            s => s.filter_has(&key(v)),
        };
        for (s, values) in &blocks {
            assert!(values.iter().all(|&v| holds(s, v)));
        }
        let up = |level: Vec<(Sum, Vec<i64>)>| -> Vec<(Sum, Vec<i64>)> {
            level
                .chunks(over)
                .map(|c| {
                    let mut s = Sum::Nothing;
                    let mut values = Vec::new();
                    for (x, v) in c {
                        s.join(x).unwrap();
                        values.extend_from_slice(v);
                    }
                    (s, values)
                })
                .collect()
        };
        let above = up(blocks);
        for (s, values) in &above {
            assert!(values.iter().all(|&v| holds(s, v)));
        }
        let top = up(above);
        for (s, values) in &top {
            assert!(values.iter().all(|&v| holds(s, v)));
        }
    }

    #[test]
    fn every_smallint_tests_present_in_its_block_s_filter_and_every_fold_above_it() {
        // Blocks of 3 sizes, so that the filters fold across 3 sizes of bits.
        walked_at_every_level(BLOOM, &[100, 700, 3000], 3);
        walked_at_every_level(
            Kind::Bloom {
                distinct: 16.0,
                rate: 0.25,
            },
            &[40, 5000],
            2,
        );
    }

    #[test]
    fn every_smallint_lies_in_its_block_s_runs_and_every_union_above_them() {
        walked_at_every_level(Kind::Multi(8), &[100, 700, 3000], 3);
        walked_at_every_level(Kind::Multi(32), &[1, 2, 40], 16);
    }

    #[test]
    fn a_filter_rules_out_most_values_it_never_held() {
        // 1,248 rows of the 124 values PostgreSQL's rule sizes the filter for.
        let sum = Sum::of(BLOOM, (0..1248).map(|v| key(v % 124 * 2)).collect());
        let false_positives = (0..1248)
            .filter(|v| sum.filter_has(&key(v * 2 + 1)))
            .count();
        assert!(false_positives * 100 <= 1248, "{false_positives}");
    }

    #[test]
    fn a_filter_every_bit_of_which_is_set_is_kept_as_one_bit() {
        let mut sum = Sum::Nothing;
        for chunk in (0..40000i64).collect::<Vec<_>>().chunks(1000) {
            sum.join(&Sum::of(BLOOM, chunk.iter().map(|&v| key(v)).collect()))
                .unwrap();
        }
        match &sum {
            Sum::Filter { bits: 1, set, .. } => assert_eq!(set, &[1]),
            other => panic!("{other:?}"),
        }
        assert!(sum.filter_has(&key(123456)));
    }

    #[test]
    fn runs_count_a_value_once_and_a_run_twice_and_join_the_nearest() {
        let keys = |v: &[i64]| v.iter().map(|&x| key(x)).collect::<Vec<_>>();
        let runs = |s: &Sum| match s {
            Sum::Runs { runs, .. } => runs
                .iter()
                .map(|(a, b)| (number_of(a).unwrap(), number_of(b).unwrap()))
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        // Eight values fit eight places.
        let eight = Sum::of(Kind::Multi(8), keys(&[1, 2, 3, 4, 100, 200, 300, 400]));
        assert_eq!(runs(&eight).len(), 8);
        // A ninth joins the nearest two, which still count two.
        let mut s = eight.clone();
        s.add(Kind::Multi(8), key(500), 9);
        let mut held: usize = 0;
        for (a, b) in runs(&s) {
            held += if a == b { 1 } else { 2 };
        }
        assert!(held <= 8, "{:?}", runs(&s));
        assert!(runs(&s).contains(&(500, 500)));
        // Far values stay apart: a run only joins the near ones.
        let spread = Sum::of(
            Kind::Multi(8),
            keys(&[1, 2, 3, 4, 5, 6, 7, 8, 1000, 1001, 1002, 1003, 1004]),
        );
        assert!(runs(&spread).iter().all(|&(a, b)| b < 1000 || a >= 1000));
        // Written and read back the same.
        let mut o = Out::default();
        spread.encode(&mut o);
        assert_eq!(o.0.len(), spread.size());
        assert_eq!(Sum::decode(&mut In::new(&o.0)).unwrap(), spread);
    }

    #[test]
    fn a_long_key_keeps_a_run_that_holds_it() {
        let long = vec![b'7'; 300];
        let s = Sum::of(Kind::Multi(8), vec![long.clone()]);
        let Sum::Runs { runs, .. } = &s else {
            panic!("{s:?}")
        };
        let (a, b) = &runs[0];
        assert!(a.as_slice() <= long.as_slice() && long.as_slice() <= b.as_slice());
        assert!(a.len() <= RUN_BOUND && b.len() <= RUN_BOUND);
    }

    #[test]
    fn a_filter_written_is_read_back_the_same() {
        let s = Sum::of(BLOOM, (0..500).map(key).collect());
        let mut o = Out::default();
        s.encode(&mut o);
        assert_eq!(Sum::decode(&mut In::new(&o.0)).unwrap(), s);
        assert_eq!(o.0.len(), s.size());
    }
}
