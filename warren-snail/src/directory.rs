//! The directory: each order's blocks as entries with their counts, packed whole into pages, with
//! levels of entries above them until one page holds the top; and each block's page map.

use crate::cut::PAGE;
use crate::file::{Extent, In, Out, Place, Reader};
use crate::places::KeyValue;
use crate::{Error, Result};

/// What an entry keeps of one of the leaves its order holds.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Summary {
    pub nulls: u32,
    /// The least and greatest value present, as their keys; both 0 where every row is NULL, and for
    /// text, which keeps neither.
    pub min: i128,
    pub max: i128,
    /// The disk pages the leaf takes under the entry, dictionaries among them.
    pub pages: u32,
    /// Whether the leaf is keyed in 128 bits, so that its least and greatest take 16 bytes each.
    pub wide: bool,
}

impl Summary {
    /// The bytes it takes in its entry.
    fn size(&self) -> usize {
        if self.wide {
            40
        } else {
            24
        }
    }
}

/// The most bytes an entry keeps of a column's least and greatest, where it takes them to part, and
/// the fewest: the first 8 lie in the leaf's summary.
const BOUND_MOST: usize = 64;
const BOUND_LEAST: usize = 8;

/// What an entry keeps past the first 8 bytes of a column's least and greatest, where those 8 do not
/// part them: as long as it takes the two to part, up to 64 bytes in all. Both are empty where 8 bytes
/// part them, or end before they do.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Tail {
    pub lo: Vec<u8>,
    pub hi: Vec<u8>,
}

static NO_TAIL: Tail = Tail {
    lo: Vec::new(),
    hi: Vec::new(),
};

impl Tail {
    /// The bytes it takes in its entry.
    fn size(&self) -> usize {
        2 + self.lo.len() + self.hi.len()
    }

    /// Cut to what it takes the two to part, past bytes both share or kept whole.
    fn parted(&mut self) {
        let shared = common(&self.lo, &self.hi);
        let keep = (shared + 1).min(BOUND_MOST - BOUND_LEAST);
        self.lo.truncate(keep);
        self.hi.truncate(keep);
    }
}

/// How many bytes two share from their first.
fn common(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

/// The bytes of a value past the first 8 a summary keeps.
fn past_kept(key: &[u8]) -> &[u8] {
    key.get(BOUND_LEAST..).unwrap_or(&[])
}

/// A column's bounds in a leaf's summary and its tail, from the least and greatest of its keys.
pub fn bounds_of(sum: &mut Summary, least: &[u8], greatest: &[u8]) -> Tail {
    sum.min = prefix_bound(least);
    sum.max = prefix_bound(greatest);
    let kept = (common(least, greatest) + 1).clamp(BOUND_LEAST, BOUND_MOST);
    if kept == BOUND_LEAST {
        return Tail::default();
    }
    let cut = |k: &[u8]| k[BOUND_LEAST..kept.min(k.len())].to_vec();
    Tail {
        lo: cut(least),
        hi: cut(greatest),
    }
}

/// A column's bounds widened to hold a key written into its block: it can only shorten what they
/// keep past the first 8 bytes.
pub fn bounds_widened(sum: &mut Summary, tail: &mut Tail, key: &[u8]) {
    let (v, past) = (prefix_bound(key), past_kept(key));
    if below_least(sum, tail, key) {
        sum.min = v;
        tail.lo = past.to_vec();
    }
    if past_greatest(sum, tail, key) {
        sum.max = v;
        tail.hi = past.to_vec();
    }
    if sum.min == sum.max {
        tail.parted();
    } else {
        *tail = Tail::default();
    }
}

/// Whether a key sorts before every value its column's bounds hold.
pub fn below_least(sum: &Summary, tail: &Tail, key: &[u8]) -> bool {
    let v = prefix_bound(key);
    v < sum.min || (v == sum.min && past_kept(key) < tail.lo.as_slice())
}

/// Whether a key sorts after every value its column's bounds hold: its bytes as far as the bound keeps
/// any lie past the bound's.
pub fn past_greatest(sum: &Summary, tail: &Tail, key: &[u8]) -> bool {
    let v = prefix_bound(key);
    let past = past_kept(key);
    v > sum.max
        || (v == sum.max
            && !tail.hi.is_empty()
            && &past[..past.len().min(tail.hi.len())] > tail.hi.as_slice())
}

/// What an entry keeps of a column a GiST or SP-GiST strand names: the least lower bound and the
/// greatest upper bound of its values, as bytes that sort as the bounds do, and whether it holds an
/// empty value.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Span {
    /// Whether it holds a value with bounds: where none, `lo` and `hi` are empty.
    pub present: bool,
    pub lo: Vec<u8>,
    pub hi: Vec<u8>,
    /// Whether it holds an empty value: an empty range or multirange.
    pub empty: bool,
}

impl Span {
    /// The bytes it takes in its entry.
    fn size(&self) -> usize {
        1 + if self.present {
            8 + self.lo.len() + self.hi.len()
        } else {
            0
        }
    }

    /// It widened to hold a value's bounds, or an empty value where none are given.
    pub fn widen(&mut self, bounds: Option<(&[u8], &[u8])>) {
        match bounds {
            None => self.empty = true,
            Some((lo, hi)) if !self.present => {
                self.present = true;
                self.lo = lo.to_vec();
                self.hi = hi.to_vec();
            }
            Some((lo, hi)) => {
                if lo < self.lo.as_slice() {
                    self.lo = lo.to_vec();
                }
                if hi > self.hi.as_slice() {
                    self.hi = hi.to_vec();
                }
            }
        }
    }

    /// It widened to hold every value another holds.
    pub fn join(&mut self, other: &Span) {
        if other.present {
            self.widen(Some((&other.lo, &other.hi)));
        }
        self.empty |= other.empty;
    }
}

/// An entry of an order's directory: a block, or above the blocks a directory page, with the counts
/// of every row under it.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Entry {
    /// The sublevels a block's place is cut to; 0 for an entry above the blocks.
    pub depth: u32,
    pub rows: u32,
    pub dead: u32,
    /// The first and last row's key, one value a level.
    pub first: Vec<KeyValue>,
    pub last: Vec<KeyValue>,
    /// Whether every key under it sorts before its last, the separator above it, where its order
    /// keeps one separator between blocks and no key just below the separator can be named.
    pub open: bool,
    /// One for each leaf the order holds, each column's in turn.
    pub columns: Vec<Summary>,
    /// The rows on each month, January first, for each of the order's month columns.
    pub months: Vec<[u32; 12]>,
    /// The rows at `-infinity` and at `infinity`, for each of the order's month columns: an
    /// infinite instant has no month.
    pub ends: Vec<[u32; 2]>,
    /// The bytes of the text present, as PostgreSQL's `octet_length` counts them, for each of the
    /// order's text columns.
    pub bytes: Vec<u32>,
    /// The span of each column a GiST or SP-GiST strand names, as the head's `span_slots` list them.
    pub spans: Vec<Span>,
    /// What it keeps past the first 8 bytes of each column the head's `bound_slots` list, where those
    /// do not part its least and greatest: none where every one's first 8 do.
    pub tails: Vec<Tail>,
    /// A block's page map, or the directory page below an entry above the blocks.
    pub child: Extent,
}

/// What an order's top page holds besides its entries.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Head {
    /// The counting unit k of each leaf the order holds.
    pub units: Vec<u32>,
    /// Which of the held leaves keep month counts, by their place among the held leaves.
    pub month_slots: Vec<u32>,
    /// Which of the held leaves are text, whose bytes each entry keeps.
    pub text_slots: Vec<u32>,
    /// For each level of the order, the sublevels it is cut on where it is text, `WIDE_LEVEL` where
    /// it is keyed in 128 bits, and 0 where it is a number.
    pub level_text: Vec<u32>,
    /// The levels of directory pages, the top among them: 1 where the top holds the blocks.
    pub height: u32,
    /// Which of the held leaves are keyed in 128 bits, by their place among the held leaves.
    pub wide_slots: Vec<u32>,
    /// For each level of the order, what it was cut under where a library orders it: its
    /// collation and the version its library gave, as `Column::cut_under` says; empty where none.
    /// An order cut under other rules than the reader's is refused by name until re-cut.
    pub cut_under: Vec<String>,
    /// Which of the held leaves are the first of a column whose span each entry keeps, by their place
    /// among the held leaves.
    pub span_slots: Vec<u32>,
    /// Which of the held leaves are the first of a column keyed by its bytes whose least and greatest
    /// each entry keeps, its first 8 bytes in the leaf's summary and any past them in its tail, by
    /// their place among the held leaves.
    pub bound_slots: Vec<u32>,
    /// Whether each entry keeps one key, the separator below it, as format 9 keeps an order: its upper
    /// bound read from the separator above it.
    pub separated: bool,
}

/// What `Head::level_text` holds for a level keyed in 128 bits.
pub const WIDE_LEVEL: u32 = 1 << 31;

/// The bit `Head::level_text` sets beside its sublevels for a level keyed by bytes that need not
/// be text: a bytea's, a bit string's.
pub const BYTES_LEVEL: u32 = 1 << 30;

/// The bit `Head::level_text` sets beside its sublevels for a level of format 9 cut a bit a rung, or
/// on its structure where it is composed, rather than a byte a rung.
pub const BITS_LEVEL: u32 = 1 << 29;

/// The top bit of a head's height, set where the order holds a column keyed in 128 bits: the
/// columns' places follow it. A head holding none is written as before.
const HEAD_WIDE: u32 = 1 << 31;

/// The bit of a head's height set where a level of the order was cut under a library: what each
/// level was cut under follows the columns keyed in 128 bits. A head with none is written as before.
const HEAD_CUT: u32 = 1 << 30;

/// The bit of a head's height set where the order keeps the span of a column a GiST or SP-GiST
/// strand names: the slots follow what each level was cut under, and each entry keeps a span for
/// each. A head with none is written as before.
const HEAD_SPANS: u32 = 1 << 29;

/// The bit of a head's height set where each entry keeps the least and greatest of a column keyed by
/// its bytes, its first 8 bytes in its first leaf's: the slots follow the spans'. A head with none is
/// written as before.
const HEAD_BOUNDS: u32 = 1 << 28;

/// The bit of a head's height set where each entry keeps one key, the separator below it, its
/// upper bound the separator above it: the next entry's, or for a page's last the entry above
/// the page's, or for the top's last the order's greatest end, which follows the bounds' slots. A
/// head without it keeps each entry's first and last keys whole.
const HEAD_SEPARATED: u32 = 1 << 27;

/// The least a level holds, which pads a key whose later levels are dropped.
const LEAST: KeyValue = KeyValue::Null { first: true };
/// The greatest a level holds.
const GREATEST: KeyValue = KeyValue::Null { first: false };

/// How many levels of a key are kept, its later levels holding the least a level holds.
pub fn kept_len(key: &[KeyValue]) -> usize {
    kept(key).len()
}

/// A key less its later levels that hold the least a level holds.
fn kept(key: &[KeyValue]) -> &[KeyValue] {
    let n = key.len() - key.iter().rev().take_while(|v| **v == LEAST).count();
    &key[..n]
}

/// A key of `levels` levels, its dropped ones holding `pad`.
fn padded(mut key: Vec<KeyValue>, levels: usize, pad: KeyValue) -> Vec<KeyValue> {
    key.resize(levels.max(key.len()), pad);
    key
}

/// The separator between two neighbouring blocks: the shortest prefix of the next block's first
/// key that sorts after the last key before it, a level of bytes or text cut to its first bytes
/// that do, text at a character's end; its dropped levels the least a level holds.
pub fn separator(last: &[KeyValue], next: &[KeyValue]) -> Vec<KeyValue> {
    let d = match last.iter().zip(next).position(|(a, b)| a != b) {
        Some(d) => d,
        None => return next.to_vec(),
    };
    let common = |a: &[u8], b: &[u8]| a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let cut = match (&last[d], &next[d]) {
        (KeyValue::Text(a), KeyValue::Text(b)) => {
            let mut j = (common(a.as_bytes(), b.as_bytes()) + 1).min(b.len());
            while !b.is_char_boundary(j) {
                j += 1;
            }
            KeyValue::Text(b[..j].into())
        }
        (KeyValue::Bytes(a), KeyValue::Bytes(b)) => {
            let j = (common(a, b) + 1).min(b.len());
            KeyValue::Bytes(b[..j].into())
        }
        (_, v) => v.clone(),
    };
    let mut s = next[..d].to_vec();
    s.push(cut);
    padded(s, next.len(), LEAST)
}

/// The greatest key a block holds, as its upper separator `s` (every key it holds sorts before
/// it) gives it: `s` less one at its last level kept where that level is a number, its later levels
/// the greatest; else `s` itself, and whether it is `s`, which no key the block holds reaches.
pub fn below(s: &[KeyValue], level_text: &[u32]) -> (Vec<KeyValue>, bool) {
    let k = kept(s);
    let Some((v, outer)) = k.split_last() else {
        return (s.to_vec(), true);
    };
    let wide = level_text.get(outer.len()) == Some(&WIDE_LEVEL);
    let less = match v {
        KeyValue::Int(x) if *x > i64::MIN => KeyValue::Int(x - 1),
        KeyValue::Wide(x) if *x > i128::MIN => KeyValue::Wide(x - 1),
        KeyValue::Int(_) | KeyValue::Wide(_) => LEAST,
        KeyValue::Null { first: false } if level_text.get(outer.len()) == Some(&0) => {
            KeyValue::Int(i64::MAX)
        }
        KeyValue::Null { first: false } if wide => KeyValue::Wide(i128::MAX),
        _ => return (s.to_vec(), true),
    };
    let mut out = outer.to_vec();
    out.push(less);
    (padded(out, s.len(), GREATEST), false)
}

/// The bytes a level of bytes or text keeps.
fn level_bytes(v: &KeyValue) -> Option<&[u8]> {
    match v {
        KeyValue::Text(s) => Some(s.as_bytes()),
        KeyValue::Bytes(b) => Some(b),
        _ => None,
    }
}

/// How many of a level's bytes an end of an order keeps, where it keeps fewer than the level holds:
/// at least 8, and as long as it takes to part from the other end's level, where the levels before
/// them are the same, capped at 64; text at a character's end.
fn end_kept(v: &KeyValue, other: Option<&KeyValue>) -> Option<usize> {
    let bytes = level_bytes(v)?;
    let parted = other
        .and_then(level_bytes)
        .map_or(0, |o| common(bytes, o) + 1);
    let kept = parted.clamp(BOUND_LEAST, BOUND_MOST);
    let at = match v {
        KeyValue::Text(s) => {
            let up = (kept..=s.len()).find(|&j| s.is_char_boundary(j));
            match up {
                Some(j) if j <= BOUND_MOST => j,
                _ if kept >= s.len() => kept,
                _ => (0..=kept)
                    .rev()
                    .find(|&j| s.is_char_boundary(j))
                    .unwrap_or(0),
            }
        }
        _ => kept,
    };
    (at < bytes.len()).then_some(at)
}

/// A key cut to bound from below what it heads, beside the order's other end `other`: each level as
/// `end_kept` keeps it, and past the first it cuts, the least a level holds.
fn cut_below(key: &[KeyValue], other: &[KeyValue]) -> Vec<KeyValue> {
    let mut out = Vec::with_capacity(key.len());
    let mut same = true;
    for (d, v) in key.iter().enumerate() {
        let across = other.get(d).filter(|_| same);
        if let Some(at) = end_kept(v, across) {
            out.push(match v {
                KeyValue::Text(s) => KeyValue::Text(s[..at].into()),
                KeyValue::Bytes(b) => KeyValue::Bytes(b[..at].into()),
                _ => unreachable!("only bytes and text are cut"),
            });
            return padded(out, key.len(), LEAST);
        }
        same = same && across == Some(v);
        out.push(v.clone());
    }
    out
}

/// The least text past every text `prefix` heads: its last character that has a next one, moved on to
/// it; none where every character is the last there is.
fn text_past(prefix: &str) -> Option<Box<str>> {
    let mut chars: Vec<char> = prefix.chars().collect();
    while let Some(c) = chars.pop() {
        let next = match c {
            '\u{D7FF}' => Some('\u{E000}'),
            c => char::from_u32(c as u32 + 1),
        };
        if let Some(n) = next {
            chars.push(n);
            return Some(chars.into_iter().collect());
        }
    }
    None
}

/// The least bytes past all that `prefix` heads: its last byte under 255 moved on by one, the rest
/// dropped; none where every byte is 255.
fn bytes_past(prefix: &[u8]) -> Option<Box<[u8]>> {
    let n = prefix.iter().rposition(|&b| b < 255)?;
    let mut out = prefix[..=n].to_vec();
    out[n] += 1;
    Some(out.into())
}

/// A key widened to bound from above what sorts up to it, beside the order's other end `other`: each
/// level as `end_kept` keeps it; a level it cuts, the least value past all its bytes head, under which
/// every key sorts, or where none is, the greatest a level holds; and whether every key sorts before
/// what it gives.
fn cut_above(key: &[KeyValue], other: &[KeyValue]) -> (Vec<KeyValue>, bool) {
    let mut out = Vec::with_capacity(key.len());
    let mut same = true;
    for (d, v) in key.iter().enumerate() {
        let across = other.get(d).filter(|_| same);
        if let Some(at) = end_kept(v, across) {
            let past = match v {
                KeyValue::Text(s) => text_past(&s[..at]).map(KeyValue::Text),
                KeyValue::Bytes(b) => bytes_past(&b[..at]).map(KeyValue::Bytes),
                _ => unreachable!("only bytes and text are cut"),
            };
            return match past {
                Some(p) => {
                    out.push(p);
                    (padded(out, key.len(), LEAST), true)
                }
                None => (padded(out, key.len(), GREATEST), false),
            };
        }
        same = same && across == Some(v);
        out.push(v.clone());
    }
    (out, false)
}

/// Neighbouring blocks written afresh, their first and last keys whole, given the bounds between
/// them: each block's first the separator below it, its last the greatest key the separator above
/// it leaves it. The first block's lower bound is `lower`, or its first key as `cut_below` cuts it
/// beside the last block's last; the last block's upper `upper`, with whether every key sorts before
/// it, or its last key as `cut_above` widens it beside the first block's first; each of the two its
/// key's where that lies beyond it.
pub fn separate(
    entries: &mut [Entry],
    level_text: &[u32],
    lower: Option<&[KeyValue]>,
    upper: Option<(&[KeyValue], bool)>,
) {
    let n = entries.len();
    if n == 0 {
        return;
    }
    let seps: Vec<Vec<KeyValue>> = (1..n)
        .map(|i| separator(&entries[i - 1].last, &entries[i].first))
        .collect();
    let first = cut_below(&entries[0].first, &entries[n - 1].last);
    let first = lower.map_or(first.clone(), |l| first.min(l.to_vec()));
    let (last, open) = cut_above(&entries[n - 1].last, &entries[0].first);
    let (last, open) = match upper {
        Some((u, under)) => match u.cmp(last.as_slice()) {
            std::cmp::Ordering::Greater => (u.to_vec(), under),
            std::cmp::Ordering::Equal => (last, open && under),
            std::cmp::Ordering::Less => (last, open),
        },
        None => (last, open),
    };
    entries[0].first = first;
    entries[n - 1].last = last;
    entries[n - 1].open = open;
    for (i, s) in seps.into_iter().enumerate() {
        (entries[i].last, entries[i].open) = below(&s, level_text);
        entries[i + 1].first = s;
    }
}

/// A block's bounds after a key written into it, where its order keeps one separator between
/// blocks: its first key lowered, as `cut_below` cuts it, or its last raised, as `cut_above` widens
/// it, each beside the bound it leaves, where the key lies beyond them; whole where it keeps both
/// keys.
pub fn widened(e: &mut Entry, key: &[KeyValue], separated: bool) {
    if key < e.first.as_slice() {
        e.first = if separated {
            cut_below(key, &e.last)
        } else {
            key.to_vec()
        };
    }
    if key > e.last.as_slice() || (e.open && key == e.last.as_slice()) {
        (e.last, e.open) = if separated {
            cut_above(key, &e.first)
        } else {
            (key.to_vec(), false)
        };
    }
}

/// What an entry above the blocks keeps past the first 8 bytes of the `k`-th column its head bounds,
/// its leaf `slot`: its entries' lowest least and highest greatest, cut to what it takes the two to
/// part, which brings a greatest back to what an entry cut short kept of it; none where their first 8
/// bytes part them.
fn over_tail(entries: &[Entry], k: usize, slot: usize) -> Tail {
    let mut held: Option<(i128, Vec<u8>, i128, Vec<u8>)> = None;
    for x in entries {
        let (c, t) = (&x.columns[slot], x.tail(k));
        if c.nulls == x.rows {
            continue;
        }
        held = Some(match held {
            None => (c.min, t.lo.clone(), c.max, t.hi.clone()),
            Some((min, lo, max, hi)) => {
                let (min, lo) = if (c.min, t.lo.as_slice()) < (min, lo.as_slice()) {
                    (c.min, t.lo.clone())
                } else {
                    (min, lo)
                };
                let (max, hi) = if (c.max, t.hi.as_slice()) > (max, hi.as_slice()) {
                    (c.max, t.hi.clone())
                } else {
                    (max, hi)
                };
                (min, lo, max, hi)
            }
        });
    }
    match held {
        Some((min, lo, max, hi)) if min == max => {
            let mut tail = Tail { lo, hi };
            tail.parted();
            tail
        }
        _ => Tail::default(),
    }
}

/// The bound an entry keeps of a value keyed by `bytes`: its first 8, a missing byte as 0, as a number
/// that sorts as they do.
pub fn prefix_bound(bytes: &[u8]) -> i128 {
    let mut b = [0u8; 8];
    let n = bytes.len().min(8);
    b[..n].copy_from_slice(&bytes[..n]);
    (u64::from_be_bytes(b) ^ (1 << 63)) as i64 as i128
}

fn add(a: u32, b: u32) -> Result<u32> {
    a.checked_add(b)
        .ok_or_else(|| Error::new("a directory entry counts more than its counts hold"))
}

/// The top bit of an entry's depth, set where its month columns hold an infinite instant: their
/// counts at each end follow their months. An entry holding none is written as format 8 writes it.
const ENTRY_ENDS: u32 = 1 << 31;

/// The next bit, set where a column's least and greatest take more than 8 bytes to part: what it keeps
/// past them follows the spans. An entry with none is written as before.
const ENTRY_TAILS: u32 = 1 << 30;

impl Entry {
    /// Whether any of its month columns holds an infinite instant.
    fn has_ends(&self) -> bool {
        self.ends.iter().any(|e| e[0] > 0 || e[1] > 0)
    }

    /// Whether a column's least and greatest take more than 8 bytes to part.
    fn has_tails(&self) -> bool {
        self.tails
            .iter()
            .any(|t| !t.lo.is_empty() || !t.hi.is_empty())
    }

    /// What it keeps past the first 8 bytes of the `k`-th column the head's `bound_slots` list.
    pub fn tail(&self, k: usize) -> &Tail {
        self.tails.get(k).unwrap_or(&NO_TAIL)
    }

    /// Its last key, and whether every key under it sorts before it.
    pub fn bound(&self) -> (&[KeyValue], bool) {
        (&self.last, self.open)
    }

    /// The bytes the entry takes in its page.
    pub fn size(&self) -> usize {
        self.size_in(false)
    }

    /// The bytes the entry takes in its page, as one key, its separator, where `separated`.
    pub fn size_in(&self, separated: bool) -> usize {
        let keys = if separated {
            32 + keys_len(kept(&self.first))
        } else {
            36 + keys_len(&self.first) + keys_len(&self.last)
        };
        keys + self.columns.iter().map(Summary::size).sum::<usize>()
            + 48 * self.months.len()
            + if self.has_ends() {
                8 * self.ends.len()
            } else {
                0
            }
            + 4 * self.bytes.len()
            + self.spans.iter().map(Span::size).sum::<usize>()
            + if self.has_tails() {
                self.tails.iter().map(Tail::size).sum()
            } else {
                0
            }
    }

    /// The entry for a directory page: the counts of the entries it holds, summed.
    pub fn over(entries: &[Entry], child: Extent, bound_slots: &[u32]) -> Result<Entry> {
        let (first, last) = match (entries.first(), entries.last()) {
            (Some(f), Some(l)) => (f, l),
            _ => return Err(Error::new("a directory page holds no entry")),
        };
        let mut e = Entry {
            open: last.open,
            depth: 0,
            rows: 0,
            dead: 0,
            first: first.first.clone(),
            last: last.last.clone(),
            columns: first
                .columns
                .iter()
                .map(|c| Summary {
                    wide: c.wide,
                    ..Summary::default()
                })
                .collect(),
            months: vec![[0u32; 12]; first.months.len()],
            ends: vec![[0u32; 2]; first.months.len()],
            bytes: vec![0u32; first.bytes.len()],
            spans: vec![Span::default(); first.spans.len()],
            tails: Vec::new(),
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
            for (m, ends) in x.ends.iter().enumerate() {
                for (i, &n) in ends.iter().enumerate() {
                    e.ends[m][i] = add(e.ends[m][i], n)?;
                }
            }
            for (t, &n) in x.bytes.iter().enumerate() {
                e.bytes[t] = add(e.bytes[t], n)?;
            }
            for (s, span) in x.spans.iter().enumerate() {
                e.spans[s].join(span);
            }
        }
        for (k, &slot) in bound_slots.iter().enumerate() {
            let tail = over_tail(entries, k, slot as usize);
            if tail != Tail::default() {
                e.tails.resize(bound_slots.len(), Tail::default());
                e.tails[k] = tail;
            }
        }
        Ok(e)
    }

    fn encode(&self, o: &mut Out, separated: bool) {
        let ends = self.has_ends();
        let tails = self.has_tails();
        o.u32(self.depth | if ends { ENTRY_ENDS } else { 0 } | if tails { ENTRY_TAILS } else { 0 });
        o.u32(self.rows);
        o.u32(self.dead);
        if separated {
            keys_out(o, kept(&self.first));
        } else {
            keys_out(o, &self.first);
            keys_out(o, &self.last);
        }
        for c in &self.columns {
            o.u32(c.nulls);
            if c.wide {
                o.i128(c.min);
                o.i128(c.max);
            } else {
                o.i64(c.min as i64);
                o.i64(c.max as i64);
            }
            o.u32(c.pages);
        }
        for m in &self.months {
            for &n in m {
                o.u32(n);
            }
        }
        if ends {
            for e in &self.ends {
                o.u32(e[0]);
                o.u32(e[1]);
            }
        }
        for &n in &self.bytes {
            o.u32(n);
        }
        for s in &self.spans {
            o.u8(u8::from(s.present) | (u8::from(s.empty) << 1));
            if s.present {
                o.bytes(&s.lo);
                o.bytes(&s.hi);
            }
        }
        if tails {
            for t in &self.tails {
                o.u8(t.lo.len() as u8);
                o.0.extend_from_slice(&t.lo);
                o.u8(t.hi.len() as u8);
                o.0.extend_from_slice(&t.hi);
            }
        }
        o.extent(self.child);
    }

    fn decode(i: &mut In, head: &Head) -> Result<Entry> {
        let depth = i.u32()?;
        let (ends, tails) = (depth & ENTRY_ENDS != 0, depth & ENTRY_TAILS != 0);
        let depth = depth & !(ENTRY_ENDS | ENTRY_TAILS);
        let rows = i.u32()?;
        let dead = i.u32()?;
        let levels = head.level_text.len();
        let (first, last) = if head.separated {
            (
                padded(keys_in(i, &head.level_text)?, levels, LEAST),
                Vec::new(),
            )
        } else {
            (keys_in(i, &head.level_text)?, keys_in(i, &head.level_text)?)
        };
        let mut columns = Vec::with_capacity(head.units.len());
        for s in 0..head.units.len() {
            let wide = head.wide_slots.contains(&(s as u32));
            let nulls = i.u32()?;
            let (min, max) = if wide {
                (i.i128()?, i.i128()?)
            } else {
                (i.i64()? as i128, i.i64()? as i128)
            };
            columns.push(Summary {
                nulls,
                min,
                max,
                pages: i.u32()?,
                wide,
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
        let mut at_ends = vec![[0u32; 2]; head.month_slots.len()];
        if ends {
            for e in at_ends.iter_mut() {
                *e = [i.u32()?, i.u32()?];
            }
        }
        let bytes = (0..head.text_slots.len())
            .map(|_| i.u32())
            .collect::<Result<Vec<_>>>()?;
        let mut spans = Vec::with_capacity(head.span_slots.len());
        for _ in 0..head.span_slots.len() {
            let flags = i.u8()?;
            let present = flags & 1 != 0;
            let (lo, hi) = if present {
                (i.bytes()?, i.bytes()?)
            } else {
                (Vec::new(), Vec::new())
            };
            spans.push(Span {
                present,
                lo,
                hi,
                empty: flags & 2 != 0,
            });
        }
        let tails = if tails {
            (0..head.bound_slots.len())
                .map(|_| {
                    let lo = i.u8()? as usize;
                    let lo = i.take(lo)?.to_vec();
                    let hi = i.u8()? as usize;
                    let hi = i.take(hi)?.to_vec();
                    Ok(Tail { lo, hi })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            Vec::new()
        };
        let child = i.extent()?;
        Ok(Entry {
            open: false,
            depth,
            rows,
            dead,
            first,
            last,
            columns,
            months,
            ends: at_ends,
            bytes,
            spans,
            tails,
            child,
        })
    }
}

/// The top bit of a key's count, set where the key holds a NULL: each level's value then follows a
/// byte saying what it is, a value (0), or a NULL before its level's values (1) or after them (2),
/// which takes no more. A key holding none is written as format 8 writes it.
const KEY_NULLS: u32 = 1 << 31;

/// A key's bytes past its count: a number in 8, text in its length and its bytes, and where the key
/// holds a NULL, a byte a level.
fn keys_len(key: &[KeyValue]) -> usize {
    let marks = if key.iter().any(KeyValue::is_null) {
        key.len()
    } else {
        0
    };
    marks
        + key
            .iter()
            .map(|v| match v {
                KeyValue::Int(_) => 8,
                KeyValue::Text(s) => 4 + s.len(),
                KeyValue::Wide(_) => 16,
                KeyValue::Bytes(b) => 4 + b.len(),
                KeyValue::Null { .. } => 0,
            })
            .sum::<usize>()
}

fn keys_out(o: &mut Out, key: &[KeyValue]) {
    let marked = key.iter().any(KeyValue::is_null);
    o.u32(key.len() as u32 | if marked { KEY_NULLS } else { 0 });
    for v in key {
        if marked {
            o.u8(match v {
                KeyValue::Null { first: true } => 1,
                KeyValue::Null { first: false } => 2,
                _ => 0,
            });
        }
        match v {
            KeyValue::Int(x) => o.i64(*x),
            KeyValue::Text(s) => o.str(s),
            KeyValue::Wide(x) => o.i128(*x),
            KeyValue::Bytes(b) => o.bytes(b),
            KeyValue::Null { .. } => {}
        }
    }
}

/// A key, its levels' kinds told by `level_text`: keyed in 128 bits where it is `WIDE_LEVEL`, by
/// bytes where it holds `BYTES_LEVEL`, else text where it is not 0.
fn keys_in(i: &mut In, level_text: &[u32]) -> Result<Vec<KeyValue>> {
    let count = i.u32()?;
    let marked = count & KEY_NULLS != 0;
    (0..(count & !KEY_NULLS) as usize)
        .map(|k| {
            match if marked { i.u8()? } else { 0 } {
                0 => {}
                1 => return Ok(KeyValue::Null { first: true }),
                2 => return Ok(KeyValue::Null { first: false }),
                other => return Err(Error::new(format!("a key's level is marked {other}"))),
            }
            Ok(match level_text.get(k).copied().unwrap_or(0) {
                WIDE_LEVEL => KeyValue::Wide(i.i128()?),
                0 => KeyValue::Int(i.i64()?),
                n if n & BYTES_LEVEL != 0 => KeyValue::Bytes(i.bytes()?.into_boxed_slice()),
                _ => KeyValue::Text(i.str()?.into_boxed_str()),
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
    let cut = head.cut_under.iter().any(|c| !c.is_empty());
    let mut height = head.height;
    if !head.wide_slots.is_empty() {
        height |= HEAD_WIDE;
    }
    if cut {
        height |= HEAD_CUT;
    }
    if !head.span_slots.is_empty() {
        height |= HEAD_SPANS;
    }
    if !head.bound_slots.is_empty() {
        height |= HEAD_BOUNDS;
    }
    if head.separated {
        height |= HEAD_SEPARATED;
    }
    o.u32(height);
    if !head.wide_slots.is_empty() {
        u32s(&mut o, &head.wide_slots);
    }
    if cut {
        o.u32(head.cut_under.len() as u32);
        for c in &head.cut_under {
            o.str(c);
        }
    }
    if !head.span_slots.is_empty() {
        u32s(&mut o, &head.span_slots);
    }
    if !head.bound_slots.is_empty() {
        u32s(&mut o, &head.bound_slots);
    }
    if head.separated {
        keys_out(
            &mut o,
            entries.last().map_or(&[][..], |e| e.last.as_slice()),
        );
        o.u8(entries.last().is_some_and(|e| e.open) as u8);
    }
    o.0.extend_from_slice(&encode_page(entries, head.separated));
    o.0
}

pub fn decode_top(buf: &[u8]) -> Result<(Head, Vec<Entry>)> {
    let mut i = In::new(buf);
    let units = read_u32s(&mut i)?;
    let month_slots = read_u32s(&mut i)?;
    let text_slots = read_u32s(&mut i)?;
    let level_text = read_u32s(&mut i)?;
    let height = i.u32()?;
    let wide_slots = if height & HEAD_WIDE != 0 {
        read_u32s(&mut i)?
    } else {
        Vec::new()
    };
    let cut_under = if height & HEAD_CUT != 0 {
        (0..i.u32()?).map(|_| i.str()).collect::<Result<Vec<_>>>()?
    } else {
        vec![String::new(); level_text.len()]
    };
    let span_slots = if height & HEAD_SPANS != 0 {
        read_u32s(&mut i)?
    } else {
        Vec::new()
    };
    let bound_slots = if height & HEAD_BOUNDS != 0 {
        read_u32s(&mut i)?
    } else {
        Vec::new()
    };
    let head = Head {
        units,
        month_slots,
        text_slots,
        level_text,
        height: height & !(HEAD_WIDE | HEAD_CUT | HEAD_SPANS | HEAD_BOUNDS | HEAD_SEPARATED),
        wide_slots,
        cut_under,
        span_slots,
        bound_slots,
        separated: height & HEAD_SEPARATED != 0,
    };
    // Where each entry keeps one key, the top's last is bounded by the order's greatest end.
    let greatest = if head.separated {
        keys_in(&mut i, &head.level_text)?
    } else {
        Vec::new()
    };
    let open = head.separated && i.u8()? != 0;
    let entries = decode_page(&buf[i.at()..], &head, (&greatest, open))?;
    Ok((head, entries))
}

/// A directory page: how many entries it holds, then the entries.
pub fn encode_page(entries: &[Entry], separated: bool) -> Vec<u8> {
    let mut o = Out::default();
    o.u32(entries.len() as u32);
    for e in entries {
        e.encode(&mut o, separated);
    }
    o.0
}

/// A directory page, its entries' kinds told by `head`. Where each entry keeps one key, an entry's
/// last is the greatest the next entry's separator leaves it, and the page's last entry's `upper`,
/// the last of the entry above the page.
pub fn decode_page(buf: &[u8], head: &Head, upper: (&[KeyValue], bool)) -> Result<Vec<Entry>> {
    let mut i = In::new(buf);
    let mut entries = (0..i.u32()?)
        .map(|_| Entry::decode(&mut i, head))
        .collect::<Result<Vec<_>>>()?;
    if head.separated {
        for k in 0..entries.len() {
            (entries[k].last, entries[k].open) = match entries.get(k + 1) {
                Some(next) => below(&next.first, &head.level_text),
                None => (
                    padded(upper.0.to_vec(), head.level_text.len(), GREATEST),
                    upper.1,
                ),
            };
        }
    }
    Ok(entries)
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
        // Where each entry keeps one key, a top of two entries or fewer is the top, a page or more.
        if top.len() <= PAGE || (head.separated && level.len() <= 2) {
            return Ok((top, below));
        }
        let mut parents = Vec::new();
        for page in pack(level, head.separated)? {
            let at = place(&encode_page(&page, head.separated))?;
            below += at.pages();
            parents.push(Entry::over(&page, at, &head.bound_slots)?);
        }
        level = parents;
        head.height += 1;
    }
}

/// Entries packed whole into directory pages, in their order, each page as full as it goes. Where each
/// entry keeps one key, a page holds two entries at least, and takes the disk pages they need.
pub fn pack(entries: Vec<Entry>, separated: bool) -> Result<Vec<Vec<Entry>>> {
    let mut pages = Vec::new();
    let mut page: Vec<Entry> = Vec::new();
    let mut used = 4;
    let least = if separated { 2 } else { 1 };
    for e in entries {
        let size = e.size_in(separated);
        if used + size > PAGE && page.len() >= least {
            pages.push(std::mem::take(&mut page));
            used = 4;
        }
        if page.is_empty() && used + size > PAGE {
            return Err(Error::new("a directory entry does not fit a page"));
        }
        used += size;
        page.push(e);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    Ok(pages)
}

#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct PageEntry {
    pub at: Extent,
    pub first_row: u32,
    pub rows: u32,
    /// The key of the page's first row: the first place it holds.
    pub first: Vec<KeyValue>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct ColumnPages {
    pub dictionary: Option<Extent>,
    pub pages: Vec<PageEntry>,
    /// The leaf's counting unit in the block, in disk pages, where its map keeps one, as a map of
    /// format 9 does; 0 where the order's head gives it.
    pub unit: u32,
}

/// The bit a page map sets on its count of leaves where it keeps each leaf's unit.
const UNITS_KEPT: u32 = 1 << 31;

/// The bytes a run of page maps begins with, in a snail of the current format: how many maps it holds.
pub(crate) const MAP_HEAD: usize = 4;

impl Entry {
    /// Where a block's page map lies: the run of pages that holds it, and where in that run it
    /// begins. The block's entry names the run's first page, and where the map begins and its bytes
    /// in the two halves of the bytes it names; a map that begins its run, as every map of format 7
    /// does, lies alone in it.
    pub(crate) fn map_run(&self) -> (Extent, usize) {
        map_run_of(self.child)
    }
}

/// `Entry::map_run`, from what a block's entry names of its page map.
pub(crate) fn map_run_of(child: Extent) -> (Extent, usize) {
    let at = (child.bytes >> 32) as usize;
    let bytes = child.bytes & u64::from(u32::MAX);
    (
        Extent {
            start: child.start,
            bytes: at as u64 + bytes,
        },
        at,
    )
}

/// What a block's entry names of its page map: the first page of the run that holds it, where in the
/// run the map begins, and its bytes.
pub(crate) fn map_child(start: u64, at: usize, bytes: usize) -> Extent {
    Extent {
        start,
        bytes: ((at as u64) << 32) | bytes as u64,
    }
}

/// A block's page map, read from the run that holds it.
pub(crate) fn read_map(reader: &Reader, e: &Entry, level_text: &[u32]) -> Result<PageMap> {
    let (run, at) = e.map_run();
    PageMap::decode(&reader.read_extent(run)?[at..], level_text)
}

/// How many page maps a run holds, from its first bytes: one where the map begins the run.
pub(crate) fn maps_held(first: &[u8], at: usize) -> u32 {
    if at == 0 {
        1
    } else {
        u32::from_le_bytes(first[..MAP_HEAD].try_into().unwrap())
    }
}

/// Page maps written as they come: in the current format packed whole into shared pages, each page
/// beginning with how many maps it holds, a map too wide for a page in a run of its own; in format 7
/// each in pages of its own. A packed page is written once no further map goes into it.
pub(crate) struct MapPages {
    packed: bool,
    open: Option<(u64, Vec<u8>, u32)>,
    /// The pages the maps take.
    pub pages: u64,
}

impl MapPages {
    pub(crate) fn new(packed: bool) -> MapPages {
        MapPages {
            packed,
            open: None,
            pages: 0,
        }
    }

    /// Places a map of `bytes` bytes, with `data` pages of its block in a run after it where it
    /// begins a run, or in a run of their own where it is packed after another map; with
    /// `same_file`, a map is packed after another only where its block's data lies in that file.
    /// Returns where the map lies, as its entry names it, and the data's first page. `write` writes
    /// the map once it is known.
    pub(crate) fn place(
        &mut self,
        w: &mut dyn Place,
        bytes: usize,
        data: u64,
        same_file: bool,
    ) -> Result<(Extent, u64)> {
        if !self.packed {
            let map_pages = bytes.div_ceil(PAGE).max(1) as u64;
            let start = w.reserve(map_pages + data);
            self.pages += map_pages;
            return Ok((map_child(start, 0, bytes), start + map_pages));
        }
        let room = self.open.as_ref().is_some_and(|(page, buf, _)| {
            buf.len() + bytes <= PAGE && (!same_file || data == 0 || w.lies_with(*page, data))
        });
        if room {
            let (page, buf, _) = self.open.as_ref().unwrap();
            let (page, at) = (*page, buf.len());
            let start = if data > 0 { w.reserve(data) } else { 0 };
            return Ok((map_child(page, at, bytes), start));
        }
        self.finish(w)?;
        let map_pages = (MAP_HEAD + bytes).div_ceil(PAGE) as u64;
        let start = w.reserve(map_pages + data);
        self.pages += map_pages;
        self.open = Some((start, vec![0u8; MAP_HEAD], 0));
        Ok((map_child(start, MAP_HEAD, bytes), start + map_pages))
    }

    /// Writes a map where `place` put it.
    pub(crate) fn write(&mut self, w: &mut dyn Place, child: Extent, map: &[u8]) -> Result<()> {
        if !self.packed {
            return w.write_at(child.start, map);
        }
        let (page, buf, n) = self
            .open
            .as_mut()
            .ok_or_else(|| Error::new("a page map was written where none was placed"))?;
        if *page != child.start || (child.bytes >> 32) as usize != buf.len() {
            return Err(Error::new(
                "a page map was written elsewhere than it was placed",
            ));
        }
        buf.extend_from_slice(map);
        *n += 1;
        buf[..MAP_HEAD].copy_from_slice(&n.to_le_bytes());
        Ok(())
    }

    /// Writes the page being packed.
    pub(crate) fn finish(&mut self, w: &mut dyn Place) -> Result<()> {
        if let Some((page, buf, n)) = self.open.take() {
            if n > 0 {
                w.write_at(page, &buf)?;
            }
        }
        Ok(())
    }
}

/// A block's page map: where each of its leaves' pages lie, and which of its rows are dead.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
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
        let kept = self.columns.iter().any(|c| c.unit != 0);
        o.u32(self.columns.len() as u32 | if kept { UNITS_KEPT } else { 0 });
        for c in &self.columns {
            match c.dictionary {
                Some(d) => {
                    o.u8(1);
                    o.extent(d);
                }
                None => o.u8(0),
            }
            if kept {
                o.u32(c.unit);
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
        let kept = n & UNITS_KEPT != 0;
        let n = n & !UNITS_KEPT;
        let mut columns = Vec::with_capacity(n as usize);
        for _ in 0..n {
            let dictionary = if i.u8()? == 1 {
                Some(i.extent()?)
            } else {
                None
            };
            let unit = if kept { i.u32()? } else { 0 };
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
            columns.push(ColumnPages {
                dictionary,
                pages,
                unit,
            });
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
            wide_slots: vec![],
            cut_under: vec![String::new(); 2],
            span_slots: Vec::new(),
            bound_slots: Vec::new(),
            separated: false,
        }
    }

    fn block(i: i64, rows: u32, child: u64) -> Entry {
        Entry {
            open: false,
            depth: 24,
            rows,
            dead: 0,
            first: ints([i, 0]),
            last: ints([i, 9]),
            columns: vec![
                Summary {
                    nulls: 0,
                    min: -(i as i128),
                    max: i as i128,
                    pages: 1,
                    ..Default::default()
                },
                Summary {
                    nulls: rows,
                    min: 0,
                    max: 0,
                    pages: 2,
                    ..Default::default()
                },
                Summary {
                    nulls: 0,
                    min: 0,
                    max: 0,
                    pages: 2,
                    ..Default::default()
                },
            ],
            months: vec![[rows, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]],
            ends: vec![[0, 0]],
            bytes: vec![10 * rows],
            spans: Vec::new(),
            tails: Vec::new(),
            child: Extent {
                start: child,
                bytes: 300,
            },
        }
    }

    #[test]
    fn an_entry_above_the_blocks_keeps_the_span_of_every_block_under_it() {
        let span = |lo: u8, hi: u8, empty: bool| Span {
            present: true,
            lo: vec![1, lo],
            hi: vec![1, hi],
            empty,
        };
        let mut a = block(1, 10, 0);
        a.spans = vec![span(5, 9, false)];
        let mut b = block(2, 10, 1);
        b.spans = vec![span(2, 6, true)];
        let over = Entry::over(&[a.clone(), b], Extent::default(), &[]).unwrap();
        assert_eq!(over.spans, [span(2, 9, true)]);
        // Written with a head that names the span's column, read back the same.
        let head = Head {
            span_slots: vec![0],
            bound_slots: vec![1],
            ..head()
        };
        let (read, entries) = decode_top(&encode_top(&head, &[a.clone(), over])).unwrap();
        assert_eq!((read.span_slots, read.bound_slots), (vec![0], vec![1]));
        assert_eq!(entries[0].spans, a.spans);
        assert_eq!(entries[1].spans, [span(2, 9, true)]);
    }

    #[test]
    fn a_top_a_page_and_a_page_map_come_back_as_written() {
        let h = head();
        let entries = vec![block(1, 3892, 40), block(2, 7, 41)];
        let mut o = Out::default();
        entries[0].encode(&mut o, false);
        assert_eq!(entries[0].size(), o.0.len());
        assert_eq!(
            decode_top(&encode_top(&h, &entries)).unwrap(),
            (h.clone(), entries.clone())
        );
        assert_eq!(
            decode_page(&encode_page(&entries, false), &h, (&[], false)).unwrap(),
            entries
        );
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
                unit: 0,
            }],
            sorted: 3890,
            dead: vec![4, 3891],
            chains: vec![(4, ints([12, 7]))],
        };
        assert_eq!(PageMap::decode(&map.encode(), &[0, 0]).unwrap(), map);
        let mut kept = map.clone();
        kept.columns[0].unit = 12;
        assert_eq!(kept.encode().len(), map.encode().len() + 4);
        assert_eq!(PageMap::decode(&kept.encode(), &[0, 0]).unwrap(), kept);
    }

    #[test]
    fn a_wide_key_and_a_wide_column_s_least_and_greatest_come_back_and_others_as_before() {
        // A head holding no wide column is written as before: the same bytes.
        let h = head();
        let entries = vec![block(1, 3892, 40)];
        let mut before = Out::default();
        u32s(&mut before, &h.units);
        u32s(&mut before, &h.month_slots);
        u32s(&mut before, &h.text_slots);
        u32s(&mut before, &h.level_text);
        before.u32(h.height);
        before.0.extend_from_slice(&encode_page(&entries, false));
        assert_eq!(encode_top(&h, &entries), before.0);
        // A uuid level and a uuid column held beside an integer.
        let wide = Head {
            units: vec![1, 1],
            month_slots: vec![],
            text_slots: vec![],
            level_text: vec![WIDE_LEVEL],
            height: 1,
            wide_slots: vec![0],
            cut_under: vec![String::new()],
            span_slots: Vec::new(),
            bound_slots: Vec::new(),
            separated: false,
        };
        let key = |k: i128| vec![KeyValue::Wide(k)];
        let e = Entry {
            open: false,
            depth: 3,
            rows: 256,
            dead: 0,
            first: key(i128::MIN),
            last: key(i128::MAX - 1),
            columns: vec![
                Summary {
                    nulls: 0,
                    min: i128::MIN,
                    max: i128::MAX - 1,
                    pages: 1,
                    wide: true,
                },
                Summary {
                    nulls: 2,
                    min: -9,
                    max: 9,
                    pages: 1,
                    wide: false,
                },
            ],
            months: vec![],
            ends: vec![],
            bytes: vec![],
            spans: Vec::new(),
            tails: Vec::new(),
            child: Extent {
                start: 7,
                bytes: 64,
            },
        };
        let mut o = Out::default();
        e.encode(&mut o, false);
        assert_eq!(e.size(), o.0.len());
        assert_eq!(
            decode_top(&encode_top(&wide, std::slice::from_ref(&e))).unwrap(),
            (wide.clone(), vec![e.clone()])
        );
        let map = PageMap {
            columns: vec![],
            sorted: 256,
            dead: vec![],
            chains: vec![(3, key(42))],
        };
        assert_eq!(
            PageMap::decode(&map.encode(), &wide.level_text).unwrap(),
            map
        );
    }

    #[test]
    fn a_key_holding_a_null_comes_back_as_written_and_one_holding_none_as_format_eight_wrote_it() {
        let text = |s: &str| KeyValue::Text(s.into());
        let (first, last) = (
            KeyValue::Null { first: true },
            KeyValue::Null { first: false },
        );
        for key in [
            vec![last.clone(), KeyValue::Int(-7), text("ab")],
            vec![KeyValue::Int(i64::MIN), first.clone(), last.clone()],
            vec![first.clone(), first.clone(), first.clone()],
            vec![KeyValue::Int(3), KeyValue::Int(4), text("")],
        ] {
            let mut o = Out::default();
            keys_out(&mut o, &key);
            assert_eq!(o.0.len(), 4 + keys_len(&key), "{key:?}");
            assert_eq!(keys_in(&mut In::new(&o.0), &[0, 0, 1]).unwrap(), key);
        }
        // A key holding no NULL: its count, then each value, as format 8 wrote it.
        let mut o = Out::default();
        keys_out(&mut o, &[KeyValue::Int(5), text("x")]);
        let mut eight = 2u32.to_le_bytes().to_vec();
        eight.extend(5i64.to_le_bytes());
        eight.extend(1u32.to_le_bytes());
        eight.push(b'x');
        assert_eq!(o.0, eight);
    }

    #[test]
    fn page_maps_are_packed_whole_into_shared_pages_and_a_wide_one_takes_a_run_of_its_own() {
        use crate::file::Stage;
        let reader = Reader::open(std::path::Path::new("/dev/null")).unwrap();
        let mut w: Stage = reader.stage();
        let mut maps = MapPages::new(true);
        // Ten maps of 1,000 bytes: four to a page, past the page's count of them.
        let small: Vec<Vec<u8>> = (0..10u8).map(|i| vec![i; 1000]).collect();
        let mut placed = Vec::new();
        for m in &small {
            let (child, data) = maps.place(&mut w, m.len(), 2, true).unwrap();
            maps.write(&mut w, child, m).unwrap();
            placed.push((child, data));
        }
        // A map wider than a page: a run of its own.
        let wide = vec![9u8; 6000];
        let (wide_child, _) = maps.place(&mut w, wide.len(), 0, true).unwrap();
        maps.write(&mut w, wide_child, &wide).unwrap();
        maps.finish(&mut w).unwrap();
        let entry = |child: Extent| Entry {
            child,
            ..block(1, 1, 0)
        };
        let pages: std::collections::BTreeSet<u64> = placed
            .iter()
            .map(|(c, _)| entry(*c).map_run().0.start)
            .collect();
        assert_eq!(pages.len(), 3);
        for ((child, _), m) in placed.iter().zip(&small) {
            let (run, at) = entry(*child).map_run();
            let buf = reader.read_extent(run).unwrap();
            assert_eq!(&buf[at..], m.as_slice());
            assert_eq!(
                maps_held(&buf, at),
                if run.start == *pages.last().unwrap() {
                    2
                } else {
                    4
                }
            );
        }
        let (run, at) = entry(wide_child).map_run();
        assert_eq!((run.pages(), at), (2, MAP_HEAD));
        let buf = reader.read_extent(run).unwrap();
        assert_eq!((maps_held(&buf, at), &buf[at..]), (1, wide.as_slice()));
        // Unpacked, as format 7 lays them, each map begins a run of its own.
        let mut seven = MapPages::new(false);
        let (child, data) = seven.place(&mut w, 1000, 2, true).unwrap();
        seven.write(&mut w, child, &small[0]).unwrap();
        assert_eq!((entry(child).map_run().1, data), (0, child.start + 1));
    }

    #[test]
    fn an_entry_above_keeps_its_children_s_counts_summed() {
        let both = Entry::over(
            &[block(3, 10, 1), block(5, 20, 2)],
            Extent {
                start: 9,
                bytes: 4096,
            },
            &[],
        )
        .unwrap();
        assert_eq!(
            both.columns[0],
            Summary {
                nulls: 0,
                min: -5,
                max: 5,
                pages: 2,
                ..Default::default()
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
            ..Default::default()
        };
        b.columns[0] = Summary {
            nulls: 1,
            min: 4,
            max: 9,
            pages: 3,
            ..Default::default()
        };
        b.months[0][4] = 6;
        let e = Entry::over(
            &[a, b],
            Extent {
                start: 9,
                bytes: 4096,
            },
            &[],
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
                pages: 4,
                ..Default::default()
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
            .flat_map(|p| decode_page(p, &h, (&[], false)).unwrap())
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

    /// Every key of a level of bytes, of up to three over 0x00, 0x01 and 0xff, and a numeric level
    /// after it of -1, 0 or 1, ascending.
    fn every_key() -> Vec<Vec<KeyValue>> {
        let mut bytes: Vec<Vec<u8>> = vec![Vec::new()];
        for len in 1..=3 {
            let mut more = Vec::new();
            for b in bytes.iter().filter(|b| b.len() == len - 1) {
                for x in [0u8, 1, 0xff] {
                    let mut c = b.clone();
                    c.push(x);
                    more.push(c);
                }
            }
            bytes.extend(more);
        }
        let mut keys: Vec<Vec<KeyValue>> = bytes
            .iter()
            .flat_map(|b| {
                [-1, 0, 1].map(|n| vec![KeyValue::Bytes(b.clone().into()), KeyValue::Int(n)])
            })
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn a_separator_bounds_the_blocks_either_side_of_it_its_prefix_heading_what_follows() {
        // Every two keys `a` before `b` of the walk, as the last key of one block and the first of
        // the next: the separator sorts after `a` and at most `b`, a prefix of `b`'s level; every
        // key at most `a` sorts before it and under the greatest it leaves the block before; every
        // key from `b` on sorts at or after it, a prefix sorting before what it heads, an ended
        // value first.
        let keys = every_key();
        let level_text = [BYTES_LEVEL | 4, 0];
        for (i, a) in keys.iter().enumerate() {
            for b in &keys[i + 1..] {
                let s = separator(a, b);
                let (under, open) = below(&s, &level_text);
                assert!(a < &s && s <= *b, "{a:?} {b:?} {s:?}");
                if let (KeyValue::Bytes(p), KeyValue::Bytes(q)) = (&s[0], &b[0]) {
                    assert!(q.starts_with(p), "{s:?} heads {b:?}");
                }
                assert!(a <= &under, "{a:?} {under:?}");
                for v in &keys {
                    if v <= a {
                        assert!(v < &s, "{v:?} {s:?}");
                        assert!(
                            if open { v < &under } else { v <= &under },
                            "{v:?} {under:?}"
                        );
                    }
                    if v >= b {
                        assert!(v >= &s && (open || v > &under), "{v:?} {s:?} {under:?}");
                    }
                }
            }
        }
        // Text is cut at a character's end.
        let t = |s: &str| vec![KeyValue::Text(s.into())];
        assert_eq!(separator(&t("ab"), &t("aé")), t("aé"));
        assert_eq!(separator(&t("a"), &t("abc")), t("ab"));
    }

    #[test]
    fn an_order_kept_one_separator_an_entry_reads_back_its_bounds_from_its_neighbours() {
        let mut entries = vec![block(1, 30, 50), block(3, 20, 51), block(7, 10, 52)];
        let level_text = [0, 0];
        separate(&mut entries, &level_text, None, None);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.first.clone(), e.last.clone()))
                .collect::<Vec<_>>(),
            [
                (ints([1, 0]), vec![KeyValue::Int(2), GREATEST]),
                (
                    vec![KeyValue::Int(3), LEAST],
                    vec![KeyValue::Int(6), GREATEST]
                ),
                (vec![KeyValue::Int(7), LEAST], ints([7, 9])),
            ]
        );
        let h = Head {
            separated: true,
            level_text: level_text.to_vec(),
            ..head()
        };
        let top = encode_top(&h, &entries);
        assert_eq!(decode_top(&top).unwrap(), (h, entries.clone()));
        // Each entry keeps one key, its separator less its dropped levels.
        let mut o = Out::default();
        entries[1].encode(&mut o, true);
        assert_eq!(
            (entries[1].size_in(true), o.0.len()),
            (entries[1].size() - 16, 164)
        );
    }

    /// Byte strings of every length to 11 over three bytes, sharing their first bytes so that the
    /// first 8 do not part them: every value a block's bounds could meet.
    fn walked() -> Vec<Vec<u8>> {
        let mut out = vec![Vec::new()];
        for head in 1..=7 {
            out.push(b"abcdefg"[..head].to_vec());
        }
        let mut tails: Vec<Vec<u8>> = vec![Vec::new()];
        for _ in 0..4 {
            let last: Vec<Vec<u8>> = tails.clone();
            for t in last {
                for b in [0u8, 1, 255] {
                    let mut v = t.clone();
                    v.push(b);
                    if !tails.contains(&v) {
                        tails.push(v);
                    }
                }
            }
        }
        for t in tails {
            let mut v = b"abcdefg".to_vec();
            v.extend(t);
            out.push(v);
        }
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn a_bound_holds_every_value_between_its_least_and_greatest_and_rules_out_what_it_parts() {
        let values = walked();
        let mut ruled_out = 0u64;
        for (i, a) in values.iter().enumerate().step_by(3) {
            for (j, z) in values.iter().enumerate().skip(i).step_by(5) {
                let mut sum = Summary::default();
                let tail = bounds_of(&mut sum, a, z);
                for (n, v) in values.iter().enumerate() {
                    let (low, high) = (below_least(&sum, &tail, v), past_greatest(&sum, &tail, v));
                    if i <= n && n <= j {
                        assert!(!low && !high, "{a:?} {z:?}: {v:?} ruled out");
                    }
                    if low {
                        assert!(v < a, "{a:?} {z:?}: {v:?} below");
                    }
                    if high {
                        assert!(v > z, "{a:?} {z:?}: {v:?} past");
                    }
                    ruled_out += (low || high) as u64;
                }
            }
        }
        assert!(ruled_out > 0);
        // What it takes to part them, no more: 15 bytes where they share 14.
        let (a, z) = (b"abcdefghijklmnA".to_vec(), b"abcdefghijklmnZ!!".to_vec());
        let mut sum = Summary::default();
        let tail = bounds_of(&mut sum, &a, &z);
        assert_eq!((tail.lo.len(), tail.hi.len()), (7, 7));
        assert!(past_greatest(&sum, &tail, b"abcdefghijklmn["));
        assert!(!past_greatest(&sum, &tail, b"abcdefghijklmnZzzzz"));
        // 8 bytes part them: nothing past them is kept, and nothing grows.
        let tail = bounds_of(&mut sum, b"abcdefgA-long", b"abcdefgZ-long");
        assert_eq!(tail, Tail::default());
        // Sharing past the cap, each keeps 64 bytes.
        let long = |c: char| format!("{}{c}", "x".repeat(80));
        let tail = bounds_of(&mut sum, long('a').as_bytes(), long('b').as_bytes());
        assert_eq!((tail.lo.len(), tail.hi.len()), (56, 56));
    }

    #[test]
    fn a_key_written_into_a_block_widens_its_bound_and_can_only_shorten_it() {
        let values = walked();
        for a in values.iter().step_by(17) {
            for z in values.iter().filter(|z| *z >= a).step_by(23) {
                for key in values.iter().step_by(11) {
                    let mut sum = Summary::default();
                    let mut tail = bounds_of(&mut sum, a, z);
                    let kept = tail.lo.len().max(tail.hi.len());
                    bounds_widened(&mut sum, &mut tail, key);
                    assert!(
                        tail.lo.len().max(tail.hi.len()) <= kept,
                        "{a:?} {z:?} {key:?}"
                    );
                    let (lo, hi) = (a.min(key), z.max(key));
                    for v in values.iter().filter(|v| *v >= lo && *v <= hi) {
                        assert!(
                            !below_least(&sum, &tail, v) && !past_greatest(&sum, &tail, v),
                            "{a:?} {z:?} {key:?}: {v:?} ruled out"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn an_entry_above_its_blocks_bounds_all_they_hold() {
        let values = walked();
        let block = |a: &[u8], z: &[u8]| {
            let mut e = block(1, 3, 1);
            e.columns = vec![Summary {
                pages: 1,
                ..Summary::default()
            }];
            e.tails = vec![bounds_of(&mut e.columns[0], a, z)];
            e
        };
        for (i, a) in values.iter().enumerate().step_by(29) {
            let z = &values[(i + 7).min(values.len() - 1)];
            for (j, b) in values.iter().enumerate().skip(i).step_by(31) {
                let y = &values[(j + 3).min(values.len() - 1)];
                let (x, w) = (block(a, z), block(b, y));
                let e = Entry::over(&[x, w], Extent::default(), &[0]).unwrap();
                for v in [a, z, b, y] {
                    assert!(
                        !below_least(&e.columns[0], e.tail(0), v)
                            && !past_greatest(&e.columns[0], e.tail(0), v),
                        "{a:?} {z:?} {b:?} {y:?}: {v:?} ruled out"
                    );
                }
            }
        }
    }

    #[test]
    fn a_greatest_cut_short_stands_for_all_it_heads_above_a_longer_one() {
        let bound = |a: &[u8], z: &[u8]| {
            let mut e = block(1, 3, 1);
            e.columns = vec![Summary {
                pages: 1,
                ..Summary::default()
            }];
            e.tails = vec![bounds_of(&mut e.columns[0], a, z)];
            e
        };
        // The first keeps `ab` past its first 8 bytes, the second `ab5`: above them both, `ab`, under
        // which the first's greatest lies, whichever comes first.
        let cut = bound(b"abcdefghaa1", b"abcdefghabz");
        let long = bound(b"abcdefghab1", b"abcdefghab5");
        for page in [[cut.clone(), long.clone()], [long, cut]] {
            let e = Entry::over(&page, Extent::default(), &[0]).unwrap();
            assert_eq!(e.tail(0).hi, b"ab", "{:?}", e.tail(0));
            assert!(!past_greatest(&e.columns[0], e.tail(0), b"abcdefghabz"));
            assert!(past_greatest(&e.columns[0], e.tail(0), b"abcdefghac"));
        }
    }

    #[test]
    fn an_order_s_two_ends_bound_all_between_them_a_level_at_a_time() {
        let keyed = |b: &[u8]| KeyValue::Bytes(b.into());
        let text = |b: &[u8]| {
            let s: String = b.iter().map(|&c| char::from(c.min(127))).collect();
            KeyValue::Text(s.into())
        };
        let values = walked();
        for wrap in [&keyed as &dyn Fn(&[u8]) -> KeyValue, &text] {
            let keys: Vec<Vec<KeyValue>> = values
                .iter()
                .map(|v| vec![wrap(v), KeyValue::Int(v.len() as i64)])
                .collect();
            let mut keys = keys;
            keys.sort();
            keys.dedup();
            for (i, a) in keys.iter().enumerate().step_by(7) {
                for (j, z) in keys.iter().enumerate().skip(i).step_by(11) {
                    let low = cut_below(a, z);
                    let (high, open) = cut_above(z, a);
                    assert!(low <= *a, "{a:?} {low:?}");
                    for v in &keys[i..=j] {
                        assert!(low <= *v, "{a:?} {z:?}: {low:?} above {v:?}");
                        assert!(
                            if open { *v < high } else { *v <= high },
                            "{a:?} {z:?}: {v:?} past {high:?} {open}"
                        );
                    }
                }
            }
        }
        // The rule's figures: 8 bytes where they part, as long as it takes where they do not, 64 at most.
        let t = |s: &str| vec![KeyValue::Text(s.into())];
        assert_eq!(
            cut_below(&t("abcdefghijkl"), &t("abcdefgzijkl")),
            t("abcdefgh")
        );
        assert_eq!(
            cut_below(&t("sharedxxx1234"), &t("sharedxxx9999")),
            t("sharedxxx1")
        );
        assert_eq!(
            cut_above(&t("sharedxxx9999"), &t("sharedxxx1234")),
            (t("sharedxxx:"), true)
        );
        let long = |c: char| format!("{}{c}", "x".repeat(80));
        assert_eq!(
            cut_below(&t(&long('a')), &t(&long('b'))),
            t(&"x".repeat(64))
        );
        assert_eq!(
            cut_above(&t(&long('b')), &t(&long('a'))),
            (t(&format!("{}y", "x".repeat(63))), true)
        );
        // A level the two share is kept whole, and the next level is cut beside its fellow.
        let two = |a: &str, b: &str| vec![KeyValue::Text(a.into()), KeyValue::Text(b.into())];
        assert_eq!(
            cut_below(&two("same", "abcdefghijA"), &two("same", "abcdefghijZ")),
            two("same", "abcdefghijA")
        );
        // Past a level they part on, 8 bytes a level.
        assert_eq!(
            cut_below(&two("a", "abcdefghijA"), &two("b", "abcdefghijZ")),
            vec![
                KeyValue::Text("a".into()),
                KeyValue::Text("abcdefgh".into())
            ]
        );
        // Text is cut at a character's end, and past the last character there is nothing.
        assert_eq!(cut_below(&t("aaaaaaaéb"), &t("aaaaaaaéc")), t("aaaaaaaéb"));
        // Written across neighbouring blocks, the order's greatest end is that bound, read as open.
        let mut ends = vec![block(1, 3, 50), block(2, 3, 51)];
        ends[0].first = t("sharedxxx1234");
        ends[0].last = t("sharedxxx4");
        ends[1].first = t("sharedxxx5");
        ends[1].last = t("sharedxxx9999");
        separate(&mut ends, &[1 | BITS_LEVEL], None, None);
        assert_eq!(ends[0].first, t("sharedxxx1"));
        assert_eq!((ends[1].last.clone(), ends[1].open), (t("sharedxxx:"), true));
        assert_eq!(text_past("\u{D7FF}"), Some("\u{E000}".into()));
        assert_eq!(text_past("a\u{10FFFF}"), Some("b".into()));
        assert_eq!(text_past("\u{10FFFF}"), None);
        assert_eq!(bytes_past(&[1, 255]), Some(vec![2u8].into()));
        assert_eq!(bytes_past(&[255, 255]), None);
    }

    #[test]
    fn a_top_keeps_whether_every_key_sorts_before_its_greatest_end() {
        let mut entries = vec![block(1, 3, 50)];
        entries[0].first = vec![KeyValue::Text("sharedxxx1".into())];
        entries[0].last = vec![KeyValue::Text("sharedxxx:".into())];
        entries[0].open = true;
        entries[0].tails = vec![Tail {
            lo: b"x1".to_vec(),
            hi: b"x:".to_vec(),
        }];
        entries[0].columns[1].min = 5;
        entries[0].columns[1].max = 5;
        let h = Head {
            separated: true,
            level_text: vec![1 | BITS_LEVEL],
            bound_slots: vec![1],
            cut_under: vec![String::new()],
            ..head()
        };
        let (read, got) = decode_top(&encode_top(&h, &entries)).unwrap();
        assert_eq!((read, got), (h, entries.clone()));
        let mut o = Out::default();
        entries[0].encode(&mut o, true);
        assert_eq!(entries[0].size_in(true), o.0.len());
    }
}
