//! What grows with the schema, once a format 9 root no longer holds it: the tables' orders and money
//! columns, the catalog's text and the sequences, each a list of entries, a key and its value, packed
//! whole into pages in the keys' order. A page is named by the first key it holds and the generation
//! that wrote it; levels stand above the entries until the top fits where it is kept, in the root or
//! in a page of its own. A commit writes afresh only the pages whose entries it changed, and those
//! above them, and a reader reads again only the pages written since it read them.

use crate::cut::PAGE;
use crate::file::{Extent, In, Out};
use crate::{Error, Result};
use std::collections::HashMap;
use std::rc::Rc;

/// A key and its value.
pub(crate) type Item = (Vec<u8>, Vec<u8>);

/// A page of a list, or its top: the level it stands at, 0 for the entries, and its items: at level 0
/// the entries, above it each page below by its first key, its page and the generation that wrote
/// it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Node {
    pub(crate) level: u8,
    pub(crate) items: Vec<Item>,
}

/// The bytes a node holding `items` takes.
pub(crate) fn node_bytes(items: &[Item]) -> usize {
    1 + 4 + items.iter().map(item_bytes).sum::<usize>()
}

fn item_bytes((k, v): &Item) -> usize {
    8 + k.len() + v.len()
}

impl Node {
    pub(crate) fn bytes(&self) -> usize {
        node_bytes(&self.items)
    }

    pub(crate) fn encode(&self, o: &mut Out) {
        o.u8(self.level);
        o.u32(self.items.len() as u32);
        for (k, v) in &self.items {
            o.bytes(k);
            o.bytes(v);
        }
    }

    pub(crate) fn decode(i: &mut In) -> Result<Node> {
        let level = i.u8()?;
        let n = i.u32()? as usize;
        let mut items = Vec::with_capacity(n.min(PAGE));
        for _ in 0..n {
            items.push((i.bytes()?, i.bytes()?));
        }
        Ok(Node { level, items })
    }
}

/// What an item above the entries holds: the page below and the generation that wrote it.
fn pointer(page: u64, at: u64) -> Vec<u8> {
    let mut v = page.to_le_bytes().to_vec();
    v.extend_from_slice(&at.to_le_bytes());
    v
}

fn pointed(v: &[u8]) -> Result<(u64, u64)> {
    let mut i = In::new(v);
    let (page, at) = (i.u64()?, i.u64()?);
    if i.at() != v.len() {
        return Err(Error::new("a list's page names no page below it"));
    }
    Ok((page, at))
}

/// A page under a list's top: its first key as the level above names it, where it lies, the
/// generation that wrote it, and what it holds.
#[derive(Clone, Debug)]
pub(crate) struct Paged {
    pub(crate) low: Vec<u8>,
    pub(crate) page: u64,
    pub(crate) at: u64,
    pub(crate) node: Rc<Node>,
}

impl Paged {
    pub(crate) fn extent(&self) -> Extent {
        Extent {
            start: self.page,
            bytes: self.node.bytes() as u64,
        }
    }
}

/// A list as it lies: its top, and the pages under it, level by level from the entries up, once
/// read.
#[derive(Clone, Debug, Default)]
pub(crate) struct Shape {
    pub(crate) top: Rc<Node>,
    pub(crate) levels: Vec<Vec<Paged>>,
}

impl Shape {
    /// A list whose top holds its entries.
    pub(crate) fn of(top: Node) -> Shape {
        Shape {
            top: Rc::new(top),
            levels: Vec::new(),
        }
    }

    /// Whether the pages under its top have been read.
    pub(crate) fn read(&self) -> bool {
        self.levels.len() == self.top.level as usize
    }

    /// Its entries, once read.
    pub(crate) fn entries(&self) -> Vec<Item> {
        match self.levels.first() {
            None => self.top.items.clone(),
            Some(leaves) => leaves
                .iter()
                .flat_map(|p| p.node.items.iter().cloned())
                .collect(),
        }
    }

    /// Every page under its top.
    pub(crate) fn pages(&self) -> impl Iterator<Item = &Paged> {
        self.levels.iter().flatten()
    }
}

/// The list under `top`, each page under it found by `page` from its number and the generation
/// that wrote it.
pub(crate) fn walk(
    top: Rc<Node>,
    page: &mut dyn FnMut(u64, u64) -> Result<Rc<Node>>,
) -> Result<Shape> {
    let mut levels: Vec<Vec<Paged>> = vec![Vec::new(); top.level as usize];
    let mut items: Vec<Item> = top.items.clone();
    for level in (0..top.level).rev() {
        let mut below = Vec::new();
        for (low, v) in items {
            let (n, at) = pointed(&v)?;
            let node = page(n, at)?;
            if node.level != level {
                return Err(Error::new(format!(
                    "page {n} of a list stands at level {}, where its top names one at {level}",
                    node.level
                )));
            }
            below.extend(node.items.iter().cloned());
            levels[level as usize].push(Paged {
                low,
                page: n,
                at,
                node,
            });
        }
        items = below;
    }
    Ok(Shape { top, levels })
}

/// The list holding `entries`, in their keys' order, written over `old`: each page whose items
/// changed written afresh by `place`, a page each, marked written at generation `at`, cut into
/// more pages where it outgrows one, and dropped where it holds none; the pages it replaced added
/// to `gone`. An item goes to the page of `old` whose first key is the greatest not above its key,
/// so a page whose entries did not change keeps its place. Levels stand above until `fits` takes
/// the bytes of the top.
pub(crate) fn settle(
    old: &Shape,
    entries: Vec<Item>,
    at: u64,
    fits: &dyn Fn(usize) -> bool,
    place: &mut dyn FnMut(&[u8]) -> Result<Extent>,
    gone: &mut Vec<Extent>,
) -> Result<Shape> {
    let mut items = entries;
    let mut levels: Vec<Vec<Paged>> = Vec::new();
    let mut level: u8 = 0;
    while !fits(node_bytes(&items)) {
        if items.len() <= 1 && level > 0 {
            return Err(Error::new("a list's top fits nowhere it is kept"));
        }
        let olds: &[Paged] = old.levels.get(level as usize).map_or(&[], |v| v);
        let mut groups: Vec<(Option<&Paged>, Vec<u8>, Vec<Item>)> = match olds {
            [] => vec![(None, Vec::new(), Vec::new())],
            _ => olds
                .iter()
                .map(|p| (Some(p), p.low.clone(), Vec::new()))
                .collect(),
        };
        let mut g = 0;
        for it in items {
            while g + 1 < groups.len() && groups[g + 1].1 <= it.0 {
                g += 1;
            }
            groups[g].2.push(it);
        }
        let mut pages = Vec::new();
        for (was, low, its) in groups {
            if let Some(p) = was {
                if p.node.items == its {
                    pages.push(p.clone());
                    continue;
                }
                gone.push(p.extent());
            }
            if its.is_empty() {
                continue;
            }
            for (j, piece) in cut_pages(its)?.into_iter().enumerate() {
                let node = Node {
                    level,
                    items: piece,
                };
                let mut o = Out::default();
                node.encode(&mut o);
                let e = place(&o.0)?;
                let low = match j {
                    0 => low.clone(),
                    _ => node.items[0].0.clone(),
                };
                pages.push(Paged {
                    low,
                    page: e.start,
                    at,
                    node: Rc::new(node),
                });
            }
        }
        items = pages
            .iter()
            .map(|p| (p.low.clone(), pointer(p.page, p.at)))
            .collect();
        levels.push(pages);
        level += 1;
    }
    // The pages of `old` at the top's level and above: the top holds their items now.
    for p in old.levels.iter().skip(level as usize).flatten() {
        gone.push(p.extent());
    }
    Ok(Shape {
        top: Rc::new(Node { level, items }),
        levels,
    })
}

/// Items cut into the fewest pages that hold them, each about as full as the others.
fn cut_pages(items: Vec<Item>) -> Result<Vec<Vec<Item>>> {
    let sizes: Vec<usize> = items.iter().map(item_bytes).collect();
    let room = PAGE - node_bytes(&[]);
    if let Some(i) = sizes.iter().position(|&s| s > room) {
        return Err(Error::new(format!(
            "an entry of {} bytes outgrows a page of the root's lists",
            sizes[i]
        )));
    }
    let mut k = 0;
    let mut used = room;
    for &s in &sizes {
        if used + s > room {
            k += 1;
            used = 0;
        }
        used += s;
    }
    loop {
        let mut cuts = Vec::with_capacity(k);
        let mut left: usize = sizes.iter().sum();
        let mut i = 0;
        for j in 0..k {
            let target = left.div_ceil(k - j);
            let start = i;
            let mut bytes = 0;
            while i < sizes.len()
                && (j + 1 == k || i == start || bytes + sizes[i] <= target)
                && bytes + sizes[i] <= room
            {
                bytes += sizes[i];
                i += 1;
            }
            left -= bytes;
            cuts.push(i);
        }
        if i == sizes.len() {
            let mut out = Vec::with_capacity(k);
            let mut rest = items;
            for w in cuts.windows(2).rev() {
                out.push(rest.split_off(rest.len() - (w[1] - w[0])));
            }
            out.push(rest);
            out.reverse();
            out.retain(|p| !p.is_empty());
            return Ok(out);
        }
        k += 1;
    }
}

/// The pages of the lists a reader has read, by their page and the generation that wrote them: the
/// pages of its last walk of each list, and of the walk before.
#[derive(Default)]
pub(crate) struct Read {
    now: HashMap<(u64, u64), Rc<Node>>,
    last: HashMap<(u64, u64), Rc<Node>>,
}

impl Read {
    /// Begins a walk: the pages of the walk before it stay found.
    pub(crate) fn begin(&mut self) {
        self.last = std::mem::take(&mut self.now);
    }

    /// The page, where a walk found it.
    pub(crate) fn found(&mut self, page: u64, at: u64) -> Option<Rc<Node>> {
        if let Some(n) = self.now.get(&(page, at)) {
            return Some(n.clone());
        }
        let n = self.last.remove(&(page, at))?;
        self.now.insert((page, at), n.clone());
        Some(n)
    }

    pub(crate) fn keep(&mut self, page: u64, at: u64, node: Rc<Node>) {
        self.now.insert((page, at), node);
    }
}

/// A list whose top is a page of its own, holding `entries`, written over the one whose top page
/// lies at `old` (its page, the generation that wrote it, and the list as read), as `settle` writes
/// it: its top page and the generation that wrote it, the old top page added to `gone` where it is
/// replaced. A list of no entries is no page.
pub(crate) fn settle_paged(
    old: Option<(Extent, u64, &Shape)>,
    entries: Vec<Item>,
    at: u64,
    place: &mut dyn FnMut(&[u8]) -> Result<Extent>,
    gone: &mut Vec<Extent>,
) -> Result<(Extent, u64)> {
    let empty = Shape::default();
    let was = old.map_or(&empty, |(_, _, s)| s);
    if entries.is_empty() {
        gone.extend(was.pages().map(Paged::extent));
        gone.extend(old.map(|(e, _, _)| e));
        return Ok((Extent::default(), 0));
    }
    let shape = settle(was, entries, at, &|b| b <= PAGE, place, gone)?;
    if let Some((e, a, s)) = old {
        if *s.top == *shape.top {
            return Ok((e, a));
        }
        gone.push(e);
    }
    let mut o = Out::default();
    shape.top.encode(&mut o);
    Ok((place(&o.0)?, at))
}

/// The text cut into entries a page each, in order.
pub(crate) fn chunks(text: &[u8]) -> Vec<Item> {
    let room = PAGE - node_bytes(&[(vec![0; 4], Vec::new())]);
    text.chunks(room)
        .enumerate()
        .map(|(i, c)| ((i as u32).to_be_bytes().to_vec(), c.to_vec()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(i: u32, width: usize) -> Item {
        (
            format!("s.table_{i:05}\0o").into_bytes(),
            vec![i as u8; width],
        )
    }

    /// Settles `entries` over `old` into pages held in `pages`, as a commit at `at` would.
    fn settled(
        old: &Shape,
        entries: Vec<Item>,
        at: u64,
        room: usize,
        pages: &mut Vec<Vec<u8>>,
    ) -> (Shape, Vec<Extent>, usize) {
        let before = pages.len();
        let mut gone = Vec::new();
        let shape = settle(
            old,
            entries,
            at,
            &|b| b <= room,
            &mut |b| {
                assert!(b.len() <= PAGE);
                pages.push(b.to_vec());
                Ok(Extent {
                    start: pages.len() as u64 - 1,
                    bytes: b.len() as u64,
                })
            },
            &mut gone,
        )
        .unwrap();
        let written = pages.len() - before;
        (shape, gone, written)
    }

    fn read_back(shape: &Shape, pages: &[Vec<u8>]) -> Shape {
        walk(shape.top.clone(), &mut |n, _| {
            Ok(Rc::new(Node::decode(&mut In::new(&pages[n as usize]))?))
        })
        .unwrap()
    }

    #[test]
    fn a_list_reads_back_its_entries_and_a_change_writes_one_path() {
        let mut pages = Vec::new();
        let entries: Vec<Item> = (0..10_000).map(|i| entry(i, 16)).collect();
        let (shape, gone, written) =
            settled(&Shape::default(), entries.clone(), 1, 2048, &mut pages);
        assert!(gone.is_empty());
        assert_eq!(shape.top.level, 2);
        assert_eq!(read_back(&shape, &pages).entries(), entries);
        let leaves = shape.levels[0].len();
        assert_eq!(written, leaves + shape.levels[1].len());
        // One entry changed: its leaf and the page above it, no other.
        let mut changed = entries.clone();
        changed[5_000].1 = vec![9; 16];
        let (again, gone, written) = settled(&shape, changed.clone(), 2, 2048, &mut pages);
        assert_eq!((written, gone.len()), (2, 2));
        assert_eq!(read_back(&again, &pages).entries(), changed);
        // One entry added beside a full leaf: that leaf cut in two, and the page above.
        let mut more = changed.clone();
        more.insert(5_001, (b"s.table_05000\0p".to_vec(), vec![1; 16]));
        let (grown, gone, written) = settled(&again, more.clone(), 3, 2048, &mut pages);
        assert_eq!(read_back(&grown, &pages).entries(), more);
        assert!(written <= 3 && gone.len() == 2, "{written} {}", gone.len());
        // Every entry gone: no page left.
        let (none, gone, written) = settled(&grown, Vec::new(), 4, 2048, &mut pages);
        assert_eq!((none.top.level, written), (0, 0));
        assert_eq!(gone.len(), grown.pages().count());
    }

    #[test]
    fn a_text_is_a_page_a_chunk_and_an_appended_one_writes_its_last() {
        let text: Vec<u8> = (0..100_000u32).map(|i| b"abcdefg;\n"[i as usize % 9]).collect();
        let mut pages = Vec::new();
        let (shape, _, written) = settled(&Shape::default(), chunks(&text), 1, PAGE, &mut pages);
        let leaves = text.len().div_ceil(PAGE - 17);
        assert_eq!((shape.levels[0].len(), written), (leaves, leaves));
        let back: Vec<u8> = read_back(&shape, &pages)
            .entries()
            .into_iter()
            .flat_map(|(_, v)| v)
            .collect();
        assert_eq!(back, text);
        let mut longer = text.clone();
        longer.extend_from_slice(b"CREATE TABLE s.t (a int);\n");
        let (_, gone, written) = settled(&shape, chunks(&longer), 2, PAGE, &mut pages);
        assert_eq!((written, gone.len()), (1, 1));
    }
}
