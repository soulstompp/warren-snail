//! The pages an open snail holds between rounds, stand-alone. When full, the page longest unused
//! goes, never one the round in progress has touched: a page that would have to push one out is read
//! and not kept. Each page keeps the tag it was read under, and the pages held under each tag are
//! counted.

use std::collections::{BTreeMap, HashMap};

/// The tag of a page read under none.
pub const UNTAGGED: u32 = u32::MAX;

struct Slot {
    tick: u64,
    round: u64,
    tag: u32,
    page: Box<[u8]>,
}

pub struct Held {
    cap: usize,
    round: u64,
    tick: u64,
    slots: HashMap<u64, Slot>,
    /// Each held page by its last use, the longest unused first.
    by_use: BTreeMap<u64, u64>,
    tagged: HashMap<u32, u64>,
}

impl Held {
    /// Holds at most `cap` pages; 0 holds none.
    pub fn new(cap: usize) -> Held {
        Held {
            cap,
            round: 0,
            tick: 0,
            slots: HashMap::new(),
            by_use: BTreeMap::new(),
            tagged: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Whether the page is held, leaving its use as it was.
    pub fn holds(&self, n: u64) -> bool {
        self.slots.contains_key(&n)
    }

    /// The pages held that were read under `tag`.
    pub fn tagged(&self, tag: u32) -> u64 {
        self.tagged.get(&tag).copied().unwrap_or(0)
    }

    pub fn begin_round(&mut self) {
        self.round += 1;
    }

    /// Drops every page held.
    pub fn clear(&mut self) {
        self.slots.clear();
        self.by_use.clear();
        self.tagged.clear();
    }

    /// Drops the pages held in `start..start + pages`.
    pub fn forget(&mut self, start: u64, pages: u64) {
        for n in start..start + pages {
            if let Some(slot) = self.slots.remove(&n) {
                self.by_use.remove(&slot.tick);
                if let Some(c) = self.tagged.get_mut(&slot.tag) {
                    *c -= 1;
                }
            }
        }
    }

    /// The page, if held, now used by this round.
    pub fn touch(&mut self, n: u64) -> Option<&[u8]> {
        let slot = self.slots.get_mut(&n)?;
        self.by_use.remove(&slot.tick);
        self.tick += 1;
        slot.tick = self.tick;
        slot.round = self.round;
        self.by_use.insert(self.tick, n);
        Some(&slot.page)
    }

    /// Keeps a page just read from the file under `tag`, unless every page held is this round's.
    pub fn keep(&mut self, n: u64, page: &[u8], tag: u32) {
        if self.cap == 0 || self.slots.contains_key(&n) {
            return;
        }
        if self.slots.len() >= self.cap {
            let (&tick, &oldest) = self
                .by_use
                .first_key_value()
                .expect("a full holding holds a page");
            if self.slots[&oldest].round == self.round {
                return;
            }
            self.by_use.remove(&tick);
            let gone = self
                .slots
                .remove(&oldest)
                .expect("a page in use order is held");
            if gone.tag != UNTAGGED {
                *self
                    .tagged
                    .get_mut(&gone.tag)
                    .expect("a held tag is counted") -= 1;
            }
        }
        self.tick += 1;
        self.slots.insert(
            n,
            Slot {
                tick: self.tick,
                round: self.round,
                tag,
                page: page.into(),
            },
        );
        self.by_use.insert(self.tick, n);
        if tag != UNTAGGED {
            *self.tagged.entry(tag).or_default() += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pages each round reads from the file, a round being the pages it touches in order.
    fn disk_pages(held: &mut Held, rounds: &[&[u64]]) -> Vec<usize> {
        rounds
            .iter()
            .map(|round| {
                held.begin_round();
                round
                    .iter()
                    .filter(|&&n| {
                        if held.touch(n).is_some() {
                            return false;
                        }
                        held.keep(n, &[n as u8], UNTAGGED);
                        true
                    })
                    .count()
            })
            .collect()
    }

    #[test]
    fn a_round_larger_than_what_is_held_keeps_what_fits() {
        let round: Vec<u64> = (10..16).collect();
        let mut held = Held::new(4);
        assert_eq!(disk_pages(&mut held, &[&round, &round, &round]), [6, 2, 2]);
        assert_eq!(held.len(), 4);
    }

    #[test]
    fn a_page_this_round_touched_is_never_pushed_out() {
        let mut held = Held::new(2);
        assert_eq!(disk_pages(&mut held, &[&[1, 2]]), [2]);
        // 3 and 4 push out the last round's 1 and 2; 5 would push out this round's 3, so it is read
        // and not kept.
        assert_eq!(disk_pages(&mut held, &[&[3, 4, 5]]), [3]);
        assert!(held.touch(3).is_some() && held.touch(4).is_some() && held.touch(5).is_none());
    }

    #[test]
    fn a_page_comes_back_as_it_was_kept_and_clear_drops_every_one() {
        let mut held = Held::new(8);
        held.begin_round();
        held.keep(5, &[1, 2, 3], UNTAGGED);
        assert_eq!(held.touch(5), Some(&[1u8, 2, 3][..]));
        held.clear();
        assert!(held.is_empty() && held.touch(5).is_none());
    }

    #[test]
    fn the_pages_held_under_a_tag_are_counted_as_they_come_and_go() {
        let mut held = Held::new(2);
        held.begin_round();
        held.keep(1, &[1], 7);
        held.keep(2, &[2], UNTAGGED);
        assert_eq!((held.tagged(7), held.tagged(UNTAGGED)), (1, 0));
        held.begin_round();
        held.keep(3, &[3], 7);
        assert_eq!(held.tagged(7), 1, "1 went for 3, both under 7");
        held.keep(4, &[4], 8);
        assert_eq!((held.tagged(7), held.tagged(8)), (1, 1), "2 went for 4");
        held.clear();
        assert_eq!(held.tagged(7), 0);
    }

    #[test]
    fn holding_nothing_keeps_nothing() {
        let mut held = Held::new(0);
        assert_eq!(disk_pages(&mut held, &[&[1, 2], &[1, 2]]), [2, 2]);
    }
}
