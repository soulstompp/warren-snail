//! The block cut, and each column's counting unit.

use crate::catalog::{Order, Table};
use crate::places::{by_equality, Keys, Sublevels};

/// The disk page, the innermost place.
pub const PAGE: usize = 4096;
/// The plain parquet library's row group, at most.
pub const ROW_GROUP_ROWS: usize = 1_048_576;
/// The plain parquet library's data page, at most, in rows and in disk pages.
pub const DATA_PAGE_ROWS: usize = 20_000;
pub const DATA_PAGE_PAGES: usize = 256;

/// One block: its place (its key's value at each sublevel, down to its depth) and its rows in
/// the order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub place: Vec<i64>,
    pub start: usize,
    pub end: usize,
}

impl Block {
    pub fn rows(&self) -> usize {
        self.end - self.start
    }
}

/// What one column of an order takes written plain: the same bytes every row, or each row's bytes
/// summed from the order's first row, so that `sum[b] - sum[a]` is what rows `a..b` take.
#[derive(Clone, Debug)]
pub enum Width {
    Fixed(usize),
    Varying(Vec<u64>),
}

/// Each column an order holds, as it is written plain.
#[derive(Clone, Debug)]
pub struct Widths(pub Vec<Width>);

impl Widths {
    /// The plain bytes rows `a..b` of the order take in column `s`.
    pub fn bytes(&self, s: usize, a: usize, b: usize) -> u64 {
        match &self.0[s] {
            Width::Fixed(w) => ((b - a) * w) as u64,
            Width::Varying(sum) => sum[b] - sum[a],
        }
    }

    /// Whether rows `a..b` fill at least one page of every column.
    pub fn fill(&self, a: usize, b: usize) -> bool {
        (0..self.0.len()).all(|s| self.bytes(s, a, b) >= PAGE as u64)
    }

    /// The bytes a row of column `s` takes, plain: its width, or the column's bytes over its `rows`.
    pub fn per_row(&self, s: usize, rows: usize) -> usize {
        match &self.0[s] {
            Width::Fixed(w) => *w,
            Width::Varying(sum) => (*sum.last().unwrap_or(&0) as usize / rows.max(1)).max(1),
        }
    }
}

/// Cuts rows sorted on an order into blocks. A place splits into its parts at the next sublevel where one
/// of them fills at least one page of every column the order holds, or where it holds more rows than
/// a row group; a place whose parts all fill less stays one block.
///
/// A part that fills no page joins the block beside it. Thin parts in a row join each other, and a
/// run of them that fills a page is a block of its own. A run that fills none joins the block before
/// it in the place, or the block after it where it comes first, unless that would pass a row group.
pub fn cut(keys: &Keys, sublevels: &Sublevels, widths: &Widths) -> Vec<Block> {
    let mut out = Vec::new();
    if keys.is_empty() {
        return out;
    }
    let mut place = Vec::new();
    go(
        keys,
        sublevels,
        widths,
        0,
        keys.len(),
        0,
        &mut place,
        &mut out,
    );
    out
}

#[allow(clippy::too_many_arguments)]
fn go(
    keys: &Keys,
    sublevels: &Sublevels,
    widths: &Widths,
    start: usize,
    end: usize,
    r: usize,
    place: &mut Vec<i64>,
    out: &mut Vec<Block>,
) {
    if r == sublevels.len() {
        out.push(Block {
            place: place.clone(),
            start,
            end,
        });
        return;
    }
    let mut parts = Vec::new();
    let mut i = start;
    while i < end {
        let v = sublevels.value(keys.row(i), r);
        let mut j = i + 1;
        while j < end && sublevels.value(keys.row(j), r) == v {
            j += 1;
        }
        parts.push((v, i, j));
        i = j;
    }
    let splits = parts.iter().any(|&(_, a, b)| widths.fill(a, b));
    if !splits && end - start <= ROW_GROUP_ROWS {
        out.push(Block {
            place: place.clone(),
            start,
            end,
        });
        return;
    }
    let first = out.len();
    let mut thin: Option<usize> = None;
    let mut after: Option<usize> = None;
    for (v, a, b) in parts {
        if widths.fill(a, b) {
            if let Some(t) = thin.take() {
                if !join_before(out, first, place, t, a) {
                    after = Some(t);
                }
            }
            let at = out.len();
            place.push(v);
            go(keys, sublevels, widths, a, b, r + 1, place, out);
            place.pop();
            if let Some(t) = after.take() {
                if out[at].rows() + (a - t) <= ROW_GROUP_ROWS {
                    out[at].start = t;
                    out[at].place = place.clone();
                } else {
                    out.insert(
                        at,
                        Block {
                            place: place.clone(),
                            start: t,
                            end: a,
                        },
                    );
                }
            }
        } else {
            let t = *thin.get_or_insert(a);
            if widths.fill(t, b) {
                out.push(Block {
                    place: place.clone(),
                    start: t,
                    end: b,
                });
                thin = None;
            }
        }
    }
    if let Some(t) = thin {
        if !join_before(out, first, place, t, end) {
            out.push(Block {
                place: place.clone(),
                start: t,
                end,
            });
        }
    }
}

/// Joins the thin rows `t..a` to the block before them, where one lies in the place starting at
/// block `first` and stays within a row group.
fn join_before(out: &mut [Block], first: usize, place: &[i64], t: usize, a: usize) -> bool {
    match out[first..].last_mut() {
        Some(last) if last.rows() + (a - t) <= ROW_GROUP_ROWS => {
            last.end = a;
            last.place = place.to_vec();
            true
        }
        _ => false,
    }
}

/// The sizes a counting unit takes, ascending, up to `bound`: 1, 2, 3, 4, 6, 8, 9, 12 and on.
pub(crate) fn unit_sizes(bound: usize) -> Vec<usize> {
    let mut out = Vec::new();
    let mut a = 1;
    while a <= bound {
        let mut b = a;
        while b <= bound {
            out.push(b);
            b *= 3;
        }
        a *= 2;
    }
    out.sort_unstable();
    out
}

/// The largest counting unit a column `width` bytes wide may take under the library's caps.
pub fn largest_unit(width: usize) -> usize {
    let fits = (DATA_PAGE_ROWS * width / PAGE).clamp(1, DATA_PAGE_PAGES);
    *unit_sizes(fits).last().unwrap()
}

/// The counting unit for `bytes`: the fewest disk pages, of the sizes a unit takes, holding them,
/// within the caps for its width.
pub fn unit(bytes: usize, width: usize) -> usize {
    let pages = bytes.div_ceil(PAGE).max(1);
    let largest = largest_unit(width);
    unit_sizes(largest)
        .into_iter()
        .find(|&k| k >= pages)
        .unwrap_or(largest)
}

/// The most plain bytes one place of the order's leading equality levels takes inside a block, column
/// by column. Where the order has no such level, a row is its own place.
pub fn largest_place(
    keys: &Keys,
    blocks: &[Block],
    order: &Order,
    table: &Table,
    widths: &Widths,
) -> Vec<u64> {
    let prefix = order
        .levels
        .iter()
        .take_while(|&&l| by_equality(l, table))
        .count();
    let mut most = vec![0u64; widths.0.len()];
    for b in blocks {
        let mut i = b.start;
        while i < b.end {
            let mut j = i + 1;
            while prefix > 0 && j < b.end && keys.row(j)[..prefix] == keys.row(i)[..prefix] {
                j += 1;
            }
            for (s, m) in most.iter_mut().enumerate() {
                *m = (*m).max(widths.bytes(s, i, j));
            }
            i = j;
        }
    }
    most
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;
    use crate::places::{ints, KeyValue};

    fn int_order() -> Catalog {
        Catalog::parse("CREATE TABLE s.t (a integer NOT NULL, PRIMARY KEY (a));").unwrap()
    }

    #[test]
    fn a_place_splits_while_every_part_fills_a_page() {
        let c = int_order();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        // 4,096 values 0..4096 of a 4-byte column: a page holds 1,024. Halves and quarters each fill a
        // page, eighths do not, so the cut stops at the quarters.
        let keys = Keys {
            stride: 1,
            data: ints(0..4096),
        };
        let blocks = cut(&keys, &sublevels, &Widths(vec![Width::Fixed(4)]));
        let rows: Vec<usize> = blocks.iter().map(|b| b.rows()).collect();
        assert_eq!(rows, [1024, 1024, 1024, 1024]);
        assert_eq!(blocks[1].place.len(), 22);
        assert_eq!(blocks[1].place[20..], [0, 1]);
    }

    fn int_blocks(data: Vec<i64>) -> Vec<Block> {
        let c = int_order();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        cut(
            &Keys {
                stride: 1,
                data: ints(data),
            },
            &sublevels,
            &Widths(vec![Width::Fixed(4)]),
        )
    }

    #[test]
    fn a_thin_part_joins_the_block_before_it_and_the_rest_splits() {
        // 3,000 values from 0 and 100 values from 1 << 20, a page holding 1,024 of them. The 100 fill
        // no page, so they join the last block of the 3,000, whose own thin end, 2,048 to 2,999,
        // joined the block before it.
        let mut data: Vec<i64> = (0..3000).collect();
        data.extend((0..100).map(|i| (1 << 20) + i));
        let blocks = int_blocks(data);
        let rows: Vec<usize> = blocks.iter().map(|b| b.rows()).collect();
        assert_eq!(rows, [1024, 1976 + 100]);
        assert_eq!(blocks[1].place.len(), 11);
    }

    #[test]
    fn a_thin_part_that_comes_first_joins_the_block_after_it() {
        let mut data: Vec<i64> = (0..100).collect();
        data.extend((0..3000).map(|i| (1 << 20) + i));
        let rows: Vec<usize> = int_blocks(data).iter().map(|b| b.rows()).collect();
        assert_eq!(rows, [100 + 1024, 1976]);
    }

    #[test]
    fn thin_parts_in_a_row_that_fill_a_page_are_a_block_of_their_own() {
        let c = Catalog::parse("CREATE TABLE s.t (s text NOT NULL, PRIMARY KEY (s));").unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[6]);
        // Each value takes 9 bytes written plain. Under `a` 2,000 values fill four pages; under each
        // of `b`, `c` and `d` 200 fill none, and the three together fill one; `e` fills two alone.
        let mut text: Vec<String> = (0..2000).map(|i| format!("a{i:04}")).collect();
        for p in ["b", "c", "d"] {
            text.extend((0..200).map(|i| format!("{p}{i:04}")));
        }
        text.extend((0..1000).map(|i| format!("e{i:04}")));
        let keys = Keys {
            stride: 1,
            data: text
                .iter()
                .map(|s| KeyValue::Text(s.as_str().into()))
                .collect(),
        };
        let widths = Widths(vec![Width::Varying(
            std::iter::once(0)
                .chain((1..=text.len() as u64).map(|n| n * 9))
                .collect(),
        )]);
        let blocks = cut(&keys, &sublevels, &widths);
        let rows: Vec<usize> = blocks.iter().map(|b| b.rows()).collect();
        assert_eq!(rows, [1000, 1000, 600, 1000]);
        assert_eq!(blocks[2].place, Vec::<i64>::new());
    }

    #[test]
    fn a_thin_part_that_would_pass_a_row_group_stands_alone() {
        // One value a row group's rows long cannot be cut, so the 100 after it stay a block of their
        // own.
        let mut data = vec![0i64; ROW_GROUP_ROWS];
        data.extend((0..100).map(|i| (1 << 20) + i));
        let rows: Vec<usize> = int_blocks(data).iter().map(|b| b.rows()).collect();
        assert_eq!(rows, [ROW_GROUP_ROWS, 100]);
    }

    /// Each row's plain bytes, summed from the first row.
    fn summed(each: impl Iterator<Item = u64>) -> Width {
        let mut sum = vec![0u64];
        for b in each {
            sum.push(sum.last().unwrap() + b);
        }
        Width::Varying(sum)
    }

    #[test]
    fn text_narrower_than_the_integers_decides_the_cut() {
        let c = Catalog::parse("CREATE TABLE s.t (a bigint NOT NULL, b text, PRIMARY KEY (a));")
            .unwrap();
        let t = &c.tables[0];
        let sublevels = Sublevels::new(&t.orders[0], t, &[0]);
        let keys = Keys {
            stride: 1,
            data: ints(0..4096),
        };
        // An 8-byte column alone: a page holds 512, so eighths fill a page and sixteenths do not.
        let alone = cut(&keys, &sublevels, &Widths(vec![Width::Fixed(8)]));
        assert_eq!(alone.len(), 8);
        // Beside it one byte of text a row, 5 written plain: a page needs 820 rows, so the quarters.
        let both = cut(
            &keys,
            &sublevels,
            &Widths(vec![Width::Fixed(8), summed((0..4096).map(|_| 5))]),
        );
        assert_eq!(
            both.iter().map(|b| b.rows()).collect::<Vec<_>>(),
            [1024, 1024, 1024, 1024]
        );
    }

    #[test]
    fn the_largest_place_is_measured_in_bytes_column_by_column() {
        let c = Catalog::parse(
            "CREATE TABLE s.t (a integer NOT NULL, b text); CREATE INDEX o ON s.t (a); ALTER TABLE s.t CLUSTER ON o;",
        )
        .unwrap();
        let t = &c.tables[0];
        let keys = Keys {
            stride: 1,
            data: ints([1, 1, 1, 2, 2]),
        };
        let text = ["xx", "y", "zzz", "wwww", "v"];
        let widths = Widths(vec![
            Width::Fixed(4),
            summed(text.iter().map(|s| s.len() as u64 + 4)),
        ]);
        let blocks = [Block {
            place: vec![],
            start: 0,
            end: 5,
        }];
        // The place a = 1 holds 3 rows: 12 bytes of a, and 6 + 5 + 7 of b against a = 2's 8 + 5.
        assert_eq!(
            largest_place(&keys, &blocks, &t.orders[0], t, &widths),
            [12, 18]
        );
    }

    #[test]
    fn the_unit_is_a_product_of_two_and_three_within_the_caps() {
        assert_eq!(unit(1, 8), 1);
        assert_eq!(unit(3892 * 8, 8), 8);
        assert_eq!(unit(5 * PAGE + 1, 8), 6);
        assert_eq!(largest_unit(8), 36);
        assert_eq!(largest_unit(4), 18);
        assert_eq!(unit(10_000_000, 8), 36);
    }
}
