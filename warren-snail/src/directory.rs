//! The directory: each order's blocks as entries with their counts, packed whole into pages, with
//! levels of entries above them until one page holds the top; and each block's page map.

use crate::cut::PAGE;
use crate::file::{Extent, In, Out};
use crate::places::KeyValue;
use crate::{Error, Result};

/// What an entry keeps of one of its columns.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub nulls: u32,
    /// The least and greatest value present; both 0 where every row is NULL, and for text, which keeps
    /// neither.
    pub min: i64,
    pub max: i64,
    /// The disk pages the column takes under the entry, dictionaries among them.
    pub pages: u32,
}

/// An entry of an order's directory: a block, or above the blocks a directory page, with the counts
/// of every row under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The sublevels a block's place is cut to; 0 for an entry above the blocks.
    pub depth: u32,
    pub rows: u32,
    pub dead: u32,
    /// The first and last row's key, one value a level.
    pub first: Vec<KeyValue>,
    pub last: Vec<KeyValue>,
    /// One for each column the order holds, in its order.
    pub columns: Vec<Summary>,
    /// The rows on each month, January first, for each of the order's month columns.
    pub months: Vec<[u32; 12]>,
    /// The bytes of the text present, as PostgreSQL's `octet_length` counts them, for each of the
    /// order's text columns.
    pub bytes: Vec<u32>,
    /// A block's page map, or the directory page below an entry above the blocks.
    pub child: Extent,
}

/// What an order's top page holds besides its entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Head {
    /// The counting unit k of each column the order holds.
    pub units: Vec<u32>,
    /// Which of the held columns keep month counts, by their place among the held columns.
    pub month_slots: Vec<u32>,
    /// Which of the held columns are text, whose bytes each entry keeps.
    pub text_slots: Vec<u32>,
    /// For each level of the order, the sublevels it is cut on where it is text, and 0 where it is a
    /// number.
    pub level_text: Vec<u32>,
    /// The levels of directory pages, the top among them: 1 where the top holds the blocks.
    pub height: u32,
}

fn add(a: u32, b: u32) -> Result<u32> {
    a.checked_add(b)
        .ok_or_else(|| Error::new("a directory entry counts more than its counts hold"))
}

impl Entry {
    /// The bytes the entry takes in its page.
    pub fn size(&self) -> usize {
        36 + keys_len(&self.first)
            + keys_len(&self.last)
            + 24 * self.columns.len()
            + 48 * self.months.len()
            + 4 * self.bytes.len()
    }

    /// The entry for a directory page: the counts of the entries it holds, summed.
    pub fn over(entries: &[Entry], child: Extent) -> Result<Entry> {
        let (first, last) = match (entries.first(), entries.last()) {
            (Some(f), Some(l)) => (f, l),
            _ => return Err(Error::new("a directory page holds no entry")),
        };
        let mut e = Entry {
            depth: 0,
            rows: 0,
            dead: 0,
            first: first.first.clone(),
            last: last.last.clone(),
            columns: vec![Summary::default(); first.columns.len()],
            months: vec![[0u32; 12]; first.months.len()],
            bytes: vec![0u32; first.bytes.len()],
            child,
        };
        let mut seen = vec![false; e.columns.len()];
        for x in entries {
            e.rows = add(e.rows, x.rows)?;
            e.dead = add(e.dead, x.dead)?;
            for (s, c) in x.columns.iter().enumerate() {
                let sum = &mut e.columns[s];
                sum.nulls = add(sum.nulls, c.nulls)?;
                sum.pages = add(sum.pages, c.pages)?;
                if c.nulls < x.rows {
                    if seen[s] {
                        sum.min = sum.min.min(c.min);
                        sum.max = sum.max.max(c.max);
                    } else {
                        sum.min = c.min;
                        sum.max = c.max;
                        seen[s] = true;
                    }
                }
            }
            for (m, months) in x.months.iter().enumerate() {
                for (i, &n) in months.iter().enumerate() {
                    e.months[m][i] = add(e.months[m][i], n)?;
                }
            }
            for (t, &n) in x.bytes.iter().enumerate() {
                e.bytes[t] = add(e.bytes[t], n)?;
            }
        }
        Ok(e)
    }

    fn encode(&self, o: &mut Out) {
        o.u32(self.depth);
        o.u32(self.rows);
        o.u32(self.dead);
        keys_out(o, &self.first);
        keys_out(o, &self.last);
        for c in &self.columns {
            o.u32(c.nulls);
            o.i64(c.min);
            o.i64(c.max);
            o.u32(c.pages);
        }
        for m in &self.months {
            for &n in m {
                o.u32(n);
            }
        }
        for &n in &self.bytes {
            o.u32(n);
        }
        o.extent(self.child);
    }

    fn decode(i: &mut In, head: &Head) -> Result<Entry> {
        let depth = i.u32()?;
        let rows = i.u32()?;
        let dead = i.u32()?;
        let first = keys_in(i, &head.level_text)?;
        let last = keys_in(i, &head.level_text)?;
        let mut columns = Vec::with_capacity(head.units.len());
        for _ in 0..head.units.len() {
            columns.push(Summary {
                nulls: i.u32()?,
                min: i.i64()?,
                max: i.i64()?,
                pages: i.u32()?,
            });
        }
        let mut months = Vec::with_capacity(head.month_slots.len());
        for _ in 0..head.month_slots.len() {
            let mut m = [0u32; 12];
            for x in m.iter_mut() {
                *x = i.u32()?;
            }
            months.push(m);
        }
        let bytes = (0..head.text_slots.len())
            .map(|_| i.u32())
            .collect::<Result<Vec<_>>>()?;
        let child = i.extent()?;
        Ok(Entry {
            depth,
            rows,
            dead,
            first,
            last,
            columns,
            months,
            bytes,
            child,
        })
    }
}

/// A key's bytes: a number in 8, text in its length and its bytes.
fn keys_len(key: &[KeyValue]) -> usize {
    key.iter()
        .map(|v| match v {
            KeyValue::Int(_) => 8,
            KeyValue::Text(s) => 4 + s.len(),
        })
        .sum()
}

fn keys_out(o: &mut Out, key: &[KeyValue]) {
    o.u32(key.len() as u32);
    for v in key {
        match v {
            KeyValue::Int(x) => o.i64(*x),
            KeyValue::Text(s) => o.str(s),
        }
    }
}

/// A key, its levels' kinds told by `level_text`: text where it is not 0.
fn keys_in(i: &mut In, level_text: &[u32]) -> Result<Vec<KeyValue>> {
    let n = i.u32()? as usize;
    (0..n)
        .map(|k| {
            Ok(if level_text.get(k).copied().unwrap_or(0) > 0 {
                KeyValue::Text(i.str()?.into_boxed_str())
            } else {
                KeyValue::Int(i.i64()?)
            })
        })
        .collect()
}

fn u32s(o: &mut Out, v: &[u32]) {
    o.u32(v.len() as u32);
    for &x in v {
        o.u32(x);
    }
}

fn read_u32s(i: &mut In) -> Result<Vec<u32>> {
    (0..i.u32()?).map(|_| i.u32()).collect()
}

/// An order's top page: its head, then its entries.
pub fn encode_top(head: &Head, entries: &[Entry]) -> Vec<u8> {
    let mut o = Out::default();
    u32s(&mut o, &head.units);
    u32s(&mut o, &head.month_slots);
    u32s(&mut o, &head.text_slots);
    u32s(&mut o, &head.level_text);
    o.u32(head.height);
    o.0.extend_from_slice(&encode_page(entries));
    o.0
}

pub fn decode_top(buf: &[u8]) -> Result<(Head, Vec<Entry>)> {
    let mut i = In::new(buf);
    let head = Head {
        units: read_u32s(&mut i)?,
        month_slots: read_u32s(&mut i)?,
        text_slots: read_u32s(&mut i)?,
        level_text: read_u32s(&mut i)?,
        height: i.u32()?,
    };
    let entries = decode_page(&buf[i.at()..], &head)?;
    Ok((head, entries))
}

/// A directory page: how many entries it holds, then the entries.
pub fn encode_page(entries: &[Entry]) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(entries.len() as u32);
    for e in entries {
        e.encode(&mut o);
    }
    o.0
}

pub fn decode_page(buf: &[u8], head: &Head) -> Result<Vec<Entry>> {
    let mut i = In::new(buf);
    (0..i.u32()?).map(|_| Entry::decode(&mut i, head)).collect()
}

/// Packs the blocks' entries whole into directory pages, level by level, until one page holds the
/// top. `place` writes a page and says where it lies. Returns the top, with its height set, and the
/// disk pages written below it.
pub fn build(
    head: Head,
    blocks: Vec<Entry>,
    place: &mut dyn FnMut(&[u8]) -> Result<Extent>,
) -> Result<(Vec<u8>, u64)> {
    build_from(head, blocks, 1, place)
}

/// Packs one level's entries, `height` levels above the blocks counting the blocks' own as 1, into
/// directory pages, level by level, until one page holds the top.
pub fn build_from(
    mut head: Head,
    entries: Vec<Entry>,
    height: u32,
    place: &mut dyn FnMut(&[u8]) -> Result<Extent>,
) -> Result<(Vec<u8>, u64)> {
    let mut level = entries;
    let mut below = 0u64;
    head.height = height;
    loop {
        let top = encode_top(&head, &level);
        if top.len() <= PAGE {
            return Ok((top, below));
        }
        let mut parents = Vec::new();
        for page in pack(level)? {
            let at = place(&encode_page(&page))?;
            below += at.pages();
            parents.push(Entry::over(&page, at)?);
        }
        level = parents;
        head.height += 1;
    }
}

/// Entries packed whole into directory pages, in their order, each page as full as it goes.
pub fn pack(entries: Vec<Entry>) -> Result<Vec<Vec<Entry>>> {
    let mut pages = Vec::new();
    let mut page: Vec<Entry> = Vec::new();
    let mut used = 4;
    for e in entries {
        if used + e.size() > PAGE && !page.is_empty() {
            pages.push(std::mem::take(&mut page));
            used = 4;
        }
        if used + e.size() > PAGE {
            return Err(Error::new("a directory entry does not fit a page"));
        }
        used += e.size();
        page.push(e);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    Ok(pages)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageEntry {
    pub at: Extent,
    pub first_row: u32,
    pub rows: u32,
    /// The key of the page's first row: the first place it holds.
    pub first: Vec<KeyValue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ColumnPages {
    pub dictionary: Option<Extent>,
    pub pages: Vec<PageEntry>,
}

/// A block's page map: where each of its columns' pages lie, and which of its rows are dead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PageMap {
    pub columns: Vec<ColumnPages>,
    /// The rows in the order's order, from the block's first; the rows after them were written into
    /// each column's last page as they came.
    pub sorted: u32,
    /// The rows that are dead, ascending.
    pub dead: Vec<u32>,
    /// Each row an update left dead, and the key of the row that replaced it on this order.
    pub chains: Vec<(u32, Vec<KeyValue>)>,
}

impl PageMap {
    /// The rows the block holds, live and dead.
    pub fn rows(&self) -> u32 {
        self.columns
            .first()
            .map_or(0, |c| c.pages.last().map_or(0, |p| p.first_row + p.rows))
    }
}

impl PageMap {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = Out::default();
        o.u32(self.columns.len() as u32);
        for c in &self.columns {
            match c.dictionary {
                Some(d) => {
                    o.u8(1);
                    o.extent(d);
                }
                None => o.u8(0),
            }
            o.u32(c.pages.len() as u32);
            for p in &c.pages {
                o.extent(p.at);
                o.u32(p.first_row);
                o.u32(p.rows);
                keys_out(&mut o, &p.first);
            }
        }
        o.u32(self.sorted);
        u32s(&mut o, &self.dead);
        o.u32(self.chains.len() as u32);
        for (r, key) in &self.chains {
            o.u32(*r);
            keys_out(&mut o, key);
        }
        o.0
    }

    /// A page map, its key's kinds told by `level_text` as in the order's head.
    pub fn decode(buf: &[u8], level_text: &[u32]) -> Result<PageMap> {
        let mut i = In::new(buf);
        let n = i.u32()?;
        let mut columns = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let dictionary = if i.u8()? == 1 {
                Some(i.extent()?)
            } else {
                None
            };
            let m = i.u32()?;
            let mut pages = Vec::with_capacity(m as usize);
            for _ in 0..m {
                pages.push(PageEntry {
                    at: i.extent()?,
                    first_row: i.u32()?,
                    rows: i.u32()?,
                    first: keys_in(&mut i, level_text)?,
                });
            }
            columns.push(ColumnPages { dictionary, pages });
        }
        let sorted = i.u32()?;
        let dead = read_u32s(&mut i)?;
        let chains = (0..i.u32()?)
            .map(|_| Ok((i.u32()?, keys_in(&mut i, level_text)?)))
            .collect::<Result<Vec<_>>>()?;
        Ok(PageMap {
            columns,
            sorted,
            dead,
            chains,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::places::ints;

    fn head() -> Head {
        Head {
            units: vec![1, 8, 1],
            month_slots: vec![1],
            text_slots: vec![2],
            level_text: vec![0, 0],
            height: 1,
        }
    }

    fn block(i: i64, rows: u32, child: u64) -> Entry {
        Entry {
            depth: 24,
            rows,
            dead: 0,
            first: ints([i, 0]),
            last: ints([i, 9]),
            columns: vec![
                Summary {
                    nulls: 0,
                    min: -i,
                    max: i,
                    pages: 1,
                },
                Summary {
                    nulls: rows,
                    min: 0,
                    max: 0,
                    pages: 2,
                },
                Summary {
                    nulls: 0,
                    min: 0,
                    max: 0,
                    pages: 2,
                },
            ],
            months: vec![[rows, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]],
            bytes: vec![10 * rows],
            child: Extent {
                start: child,
                bytes: 300,
            },
        }
    }

    #[test]
    fn a_top_a_page_and_a_page_map_come_back_as_written() {
        let h = head();
        let entries = vec![block(1, 3892, 40), block(2, 7, 41)];
        let mut o = Out::default();
        entries[0].encode(&mut o);
        assert_eq!(entries[0].size(), o.0.len());
        assert_eq!(
            decode_top(&encode_top(&h, &entries)).unwrap(),
            (h.clone(), entries.clone())
        );
        assert_eq!(decode_page(&encode_page(&entries), &h).unwrap(), entries);
        let map = PageMap {
            columns: vec![ColumnPages {
                dictionary: Some(Extent {
                    start: 41,
                    bytes: 100,
                }),
                pages: vec![PageEntry {
                    at: Extent {
                        start: 42,
                        bytes: 4000,
                    },
                    first_row: 0,
                    rows: 3892,
                    first: ints([12, -5]),
                }],
            }],
            sorted: 3890,
            dead: vec![4, 3891],
            chains: vec![(4, ints([12, 7]))],
        };
        assert_eq!(PageMap::decode(&map.encode(), &[0, 0]).unwrap(), map);
    }

    #[test]
    fn an_entry_above_keeps_its_children_s_counts_summed() {
        let both = Entry::over(
            &[block(3, 10, 1), block(5, 20, 2)],
            Extent {
                start: 9,
                bytes: 4096,
            },
        )
        .unwrap();
        assert_eq!(
            both.columns[0],
            Summary {
                nulls: 0,
                min: -5,
                max: 5,
                pages: 2
            }
        );
        let mut a = block(3, 10, 1);
        let mut b = block(5, 20, 2);
        // A column every row of `a` leaves NULL says nothing of the least and greatest.
        a.columns[0] = Summary {
            nulls: 10,
            min: 0,
            max: 0,
            pages: 1,
        };
        b.columns[0] = Summary {
            nulls: 1,
            min: 4,
            max: 9,
            pages: 3,
        };
        b.months[0][4] = 6;
        let e = Entry::over(
            &[a, b],
            Extent {
                start: 9,
                bytes: 4096,
            },
        )
        .unwrap();
        assert_eq!((e.depth, e.rows, e.dead), (0, 30, 0));
        assert_eq!(
            (e.first.clone(), e.last.clone()),
            (ints([3, 0]), ints([5, 9]))
        );
        assert_eq!(
            e.columns[0],
            Summary {
                nulls: 11,
                min: 4,
                max: 9,
                pages: 4
            }
        );
        assert_eq!(e.columns[1].nulls, 30);
        assert_eq!(e.months[0], [30, 0, 0, 0, 6, 0, 0, 0, 0, 0, 0, 2]);
        assert_eq!(e.bytes, [300]);
        assert_eq!(
            e.child,
            Extent {
                start: 9,
                bytes: 4096
            }
        );
    }

    #[test]
    fn entries_pack_whole_into_pages_and_levels_rise_until_one_page_holds_the_top() {
        // Entries of 192 bytes: 21 to a page. 1,000 of them make 48 pages, which make 3 above them, and
        // those fit the top.
        let blocks: Vec<Entry> = (0..1000).map(|i| block(i, 1000, i as u64)).collect();
        assert_eq!(blocks[0].size(), 192);
        let mut pages: Vec<Vec<u8>> = Vec::new();
        let mut next = 100u64;
        let (top, below) = build(head(), blocks.clone(), &mut |bytes| {
            pages.push(bytes.to_vec());
            let at = Extent {
                start: next,
                bytes: bytes.len() as u64,
            };
            next += at.pages();
            Ok(at)
        })
        .unwrap();
        assert!(top.len() <= PAGE && pages.iter().all(|p| p.len() <= PAGE));
        assert_eq!(pages.len(), 48 + 3);
        assert_eq!(below, 51);
        let (h, entries) = decode_top(&top).unwrap();
        assert_eq!((h.height, entries.len()), (3, 3));
        assert_eq!(entries.iter().map(|e| e.rows).sum::<u32>(), 1_000_000);
        // The leaves, read back in order, are the blocks as written.
        let leaves: Vec<Entry> = pages[..48]
            .iter()
            .flat_map(|p| decode_page(p, &h).unwrap())
            .collect();
        assert_eq!(leaves, blocks);

        // 30 entries take two pages, so they never sit in the top together.
        let (top, below) = build(head(), blocks[..30].to_vec(), &mut |bytes| {
            Ok(Extent {
                start: 0,
                bytes: bytes.len() as u64,
            })
        })
        .unwrap();
        assert!(top.len() <= PAGE);
        assert_eq!((decode_top(&top).unwrap().0.height, below), (2, 2));
    }
}
