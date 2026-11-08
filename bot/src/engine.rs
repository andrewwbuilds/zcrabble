//! Word engine: which dictionary words a rack can spell, and the best set of
//! disjoint words (max total reward) a rack can be cashed out for.
//!
//! Rewards follow the module exactly (`letters.ts`, `playWord` in `game.ts`):
//! base = sum of tile values; reward = base * mult (integer division) with
//! mult 1x for <=3 letters, 1.5x at 4, 2x at 5, 2.5x at 6, 3x at 7+.

pub const VAL: [u32; 26] = [
    1, 3, 3, 2, 1, 4, 2, 4, 1, 8, 5, 1, 3, 1, 1, 3, 10, 1, 1, 1, 1, 4, 4, 8, 4, 10,
];

/// Standard Scrabble bag without blanks (98 tiles), as `DEFAULT_BAG`.
pub const BAG: [u8; 26] = [
    9, 2, 2, 4, 12, 2, 3, 2, 9, 1, 1, 4, 2, 6, 8, 2, 1, 6, 4, 6, 4, 2, 2, 1, 2, 1,
];

pub type R8 = [u8; 26];

pub fn reward_for(base: u32, len: usize) -> u32 {
    match len {
        0..=3 => base,
        4 => base * 3 / 2,
        5 => base * 2,
        6 => base * 5 / 2,
        _ => base * 3,
    }
}

pub fn idx(c: char) -> Option<usize> {
    let u = c.to_ascii_uppercase();
    u.is_ascii_uppercase().then(|| (u as u8 - b'A') as usize)
}

pub fn ch(i: usize) -> char {
    (b'A' + i as u8) as char
}

pub fn rack_total(r: &R8) -> usize {
    r.iter().map(|&c| c as usize).sum()
}

pub fn rack_face(r: &R8) -> u32 {
    r.iter().zip(VAL.iter()).map(|(&c, &v)| c as u32 * v).sum()
}

pub fn rack_mask(r: &R8) -> u32 {
    let mut m = 0u32;
    for (i, &c) in r.iter().enumerate() {
        if c > 0 {
            m |= 1 << i;
        }
    }
    m
}

pub fn rack_str(r: &R8) -> String {
    let mut s = String::new();
    for (i, &c) in r.iter().enumerate() {
        for _ in 0..c {
            s.push(ch(i));
        }
    }
    s
}

#[inline]
pub fn fits(w: &R8, r: &R8) -> bool {
    w.iter().zip(r.iter()).all(|(a, b)| a <= b)
}

#[inline]
pub fn sub(r: &mut R8, w: &R8) {
    for i in 0..26 {
        r[i] -= w[i];
    }
}

#[inline]
pub fn add(r: &mut R8, w: &R8) {
    for i in 0..26 {
        r[i] += w[i];
    }
}

pub struct Word {
    pub counts: R8,
    pub mask: u32,
    pub len: u8,
    pub reward: u32,
    pub text: String,
}

pub struct Engine {
    /// Sorted by reward desc, then length desc, then text.
    pub words: Vec<Word>,
}

impl Engine {
    pub fn from_wordlist(src: &str) -> Engine {
        let mut words: Vec<Word> = Vec::new();
        for line in src.lines() {
            let w = line.trim().to_ascii_uppercase();
            if w.len() < 2 || !w.bytes().all(|b| b.is_ascii_uppercase()) {
                continue;
            }
            let mut counts = [0u8; 26];
            let mut base = 0u32;
            for b in w.bytes() {
                let i = (b - b'A') as usize;
                counts[i] = counts[i].saturating_add(1);
                base += VAL[i];
            }
            words.push(Word {
                mask: rack_mask(&counts),
                len: w.len().min(255) as u8,
                reward: reward_for(base, w.len()),
                counts,
                text: w,
            });
        }
        words.sort_by(|a, b| {
            b.reward
                .cmp(&a.reward)
                .then(b.len.cmp(&a.len))
                .then(a.text.cmp(&b.text))
        });
        words.dedup_by(|a, b| a.text == b.text);
        Engine { words }
    }

    pub fn load() -> Engine {
        Engine::from_wordlist(include_str!("../wordlist.txt"))
    }

    /// Indices of playable words with length >= `min_len`, best reward first,
    /// at most `max_n` of them.
    pub fn playable(&self, rack: &R8, min_len: usize, max_n: usize, out: &mut Vec<u32>) {
        out.clear();
        let mask = rack_mask(rack);
        let total = rack_total(rack);
        if total < min_len {
            return;
        }
        for (i, w) in self.words.iter().enumerate() {
            if w.mask & !mask != 0 {
                continue;
            }
            let l = w.len as usize;
            if l > total || l < min_len {
                continue;
            }
            if fits(&w.counts, rack) {
                out.push(i as u32);
                if out.len() >= max_n {
                    break;
                }
            }
        }
    }

    /// Best set of disjoint words (max total reward) from `rack`.
    pub fn best_set(&self, rack: &R8, max_cands: usize, node_limit: u32) -> (u32, Vec<u32>) {
        let mut cands = Vec::with_capacity(max_cands);
        self.playable(rack, 2, max_cands, &mut cands);
        let mut st = Dfs {
            eng: self,
            cands: &cands,
            best: 0,
            best_v: Vec::new(),
            cur: Vec::new(),
            nodes: 0,
            limit: node_limit,
        };
        let mut r = *rack;
        st.go(0, &mut r, 0);
        (st.best, st.best_v)
    }

    /// Just the value of the best set.
    pub fn value(&self, rack: &R8, max_cands: usize, node_limit: u32) -> u32 {
        self.best_set(rack, max_cands, node_limit).0
    }

    pub fn word(&self, i: u32) -> &Word {
        &self.words[i as usize]
    }
}

struct Dfs<'a> {
    eng: &'a Engine,
    cands: &'a [u32],
    best: u32,
    best_v: Vec<u32>,
    cur: Vec<u32>,
    nodes: u32,
    limit: u32,
}

impl Dfs<'_> {
    fn go(&mut self, start: usize, rack: &mut R8, acc: u32) {
        self.nodes += 1;
        if acc > self.best {
            self.best = acc;
            self.best_v = self.cur.clone();
        }
        if self.nodes > self.limit {
            return;
        }
        // Upper bound: every remaining tile scores at most 3x its face.
        if acc + 3 * rack_face(rack) <= self.best {
            return;
        }
        for j in start..self.cands.len() {
            let wi = self.cands[j];
            let w = &self.eng.words[wi as usize];
            if !fits(&w.counts, rack) {
                continue;
            }
            sub(rack, &w.counts);
            self.cur.push(wi);
            self.go(j, rack, acc + w.reward);
            self.cur.pop();
            add(rack, &w.counts);
            if self.nodes > self.limit {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rack(s: &str) -> R8 {
        let mut r = [0u8; 26];
        for c in s.chars() {
            r[idx(c).unwrap()] += 1;
        }
        r
    }

    #[test]
    fn rewards_match_module() {
        assert_eq!(reward_for(5, 3), 5);
        assert_eq!(reward_for(5, 4), 7);
        assert_eq!(reward_for(5, 5), 10);
        assert_eq!(reward_for(5, 6), 12);
        assert_eq!(reward_for(5, 7), 15);
        assert_eq!(reward_for(5, 9), 15);
    }

    #[test]
    fn best_set_finds_two_words() {
        let e = Engine::load();
        let (v, set) = e.best_set(&rack("RETAINSQUIZ"), 200, 20000);
        let words: Vec<&str> = set.iter().map(|&i| e.word(i).text.as_str()).collect();
        assert!(v > 0, "{words:?}");
        let mut used = [0u8; 26];
        for &i in &set {
            add(&mut used, &e.word(i).counts);
        }
        assert!(fits(&used, &rack("RETAINSQUIZ")));
        println!("{v} {words:?}");
    }
}
