//! The module's dictionary (ENABLE, embedded from `bot/wordlist.txt`, a copy of
//! `spacetimedb/wordlist.txt`) plus the tile values and word rewards the module
//! uses (`spacetimedb/src/letters.ts`, `playWord` in `game.ts`).

/// Letter counts, index 0 = 'A' .. 25 = 'Z'.
pub type Rack = [u32; 26];

static WORDLIST: &str = include_str!("../wordlist.txt");

/// Standard Scrabble tile values (no blanks), as `letterValue` in letters.ts.
pub fn letter_value(c: char) -> i64 {
    match c {
        'A' | 'E' | 'I' | 'O' | 'U' | 'L' | 'N' | 'S' | 'T' | 'R' => 1,
        'D' | 'G' => 2,
        'B' | 'C' | 'M' | 'P' => 3,
        'F' | 'H' | 'V' | 'W' | 'Y' => 4,
        'K' => 5,
        'J' | 'X' => 8,
        'Q' | 'Z' => 10,
        _ => 0,
    }
}

/// What the module credits for playing `word` (uppercase A-Z): base score times
/// the length multiplier (1x <=3, 1.5x at 4, 2x at 5, 2.5x at 6, 3x at 7+),
/// with the module's integer division.
pub fn word_reward(word: &str) -> i64 {
    let base: i64 = word.chars().map(letter_value).sum();
    let (num, den) = match word.len() {
        0..=3 => (1, 1),
        4 => (3, 2),
        5 => (2, 1),
        6 => (5, 2),
        _ => (3, 1),
    };
    base * num / den
}

pub fn letter_index(c: char) -> Option<usize> {
    c.is_ascii_uppercase().then(|| (c as u8 - b'A') as usize)
}

/// Adds `n` tiles of `letter` (first char, uppercase) to `rack`.
pub fn rack_add(rack: &mut Rack, letter: &str, n: u32) {
    if let Some(i) = letter.chars().next().and_then(letter_index) {
        rack[i] += n;
    }
}

pub fn rack_size(rack: &Rack) -> u32 {
    rack.iter().sum()
}

/// The rack as letters in A-Z order, e.g. "AEENRST".
pub fn rack_string(rack: &Rack) -> String {
    let mut s = String::new();
    for (i, &n) in rack.iter().enumerate() {
        for _ in 0..n {
            s.push((b'A' + i as u8) as char);
        }
    }
    s
}

/// Whether `rack` holds every letter of `word`, counting repeats.
pub fn can_spell(word: &str, rack: &Rack) -> bool {
    let mut need = [0u32; 26];
    for c in word.chars() {
        match letter_index(c) {
            Some(i) => {
                need[i] += 1;
                if need[i] > rack[i] {
                    return false;
                }
            }
            None => return false,
        }
    }
    true
}

fn letter_mask(word: &str) -> u32 {
    word.bytes()
        .filter(u8::is_ascii_uppercase)
        .fold(0, |m, b| m | 1 << (b - b'A'))
}

struct Entry {
    word: &'static str,
    mask: u32,
}

pub struct Dictionary {
    /// Byte-order sorted, for `contains` (the module binary-searches the same list).
    lex: Vec<&'static str>,
    /// Longest first, then lexicographic, like the module's `wordsByLength`.
    by_len: Vec<Entry>,
    /// `by_len[first_with_len_at_most[n]..]` holds the words of length <= n.
    first_with_len_at_most: Vec<usize>,
}

impl Dictionary {
    pub fn load() -> Self {
        let mut lex: Vec<&'static str> = WORDLIST
            .lines()
            .map(str::trim)
            .filter(|w| w.len() >= 2 && w.bytes().all(|b| b.is_ascii_uppercase()))
            .collect();
        lex.sort_unstable();
        lex.dedup();
        let mut by_len: Vec<Entry> = lex
            .iter()
            .map(|&word| Entry {
                word,
                mask: letter_mask(word),
            })
            .collect();
        by_len.sort_by(|a, b| b.word.len().cmp(&a.word.len()).then(a.word.cmp(b.word)));
        let max_len = by_len.first().map_or(0, |e| e.word.len());
        let first_with_len_at_most = (0..=max_len)
            .map(|n| by_len.partition_point(|e| e.word.len() > n))
            .collect();
        Self {
            lex,
            by_len,
            first_with_len_at_most,
        }
    }

    pub fn len(&self) -> usize {
        self.lex.len()
    }

    /// Exact membership; `word` must already be uppercase A-Z.
    pub fn contains(&self, word: &str) -> bool {
        self.lex.binary_search(&word).is_ok()
    }

    /// The longest word (ties: lexicographically first) spellable from `rack`
    /// with at least `min_len` letters.
    pub fn longest_playable(&self, rack: &Rack, min_len: usize) -> Option<&'static str> {
        let total = rack_size(rack) as usize;
        if total < min_len.max(2) {
            return None;
        }
        let rack_mask = rack
            .iter()
            .enumerate()
            .filter(|(_, &n)| n > 0)
            .fold(0u32, |m, (i, _)| m | 1 << i);
        let start = self
            .first_with_len_at_most
            .get(total)
            .copied()
            .unwrap_or(0);
        for e in &self.by_len[start..] {
            if e.word.len() < min_len {
                break;
            }
            if e.mask & !rack_mask == 0 && can_spell(e.word, rack) {
                return Some(e.word);
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rack_of(s: &str) -> Rack {
        let mut r = [0; 26];
        for c in s.chars() {
            rack_add(&mut r, &c.to_string(), 1);
        }
        r
    }

    #[test]
    fn rewards_match_module() {
        // HOVE: base 10, 4 letters x1.5 = 15; FROCK: 14 x2 = 28; AXE: 10 x1.
        assert_eq!(word_reward("HOVE"), 15);
        assert_eq!(word_reward("FROCK"), 28);
        assert_eq!(word_reward("AXE"), 10);
        // 6 letters: x2.5 with integer division (base 7 -> 17).
        assert_eq!(word_reward("ARISEN"), 15);
        assert_eq!(word_reward("ZAX"), 19);
    }

    #[test]
    fn dictionary_lookup_and_search() {
        let d = Dictionary::load();
        assert!(d.len() > 170_000);
        assert!(d.contains("AA") && d.contains("ZYZZYVAS") && d.contains("QAT") && d.contains("XU"));
        // ENABLE predates QI and ZA.
        assert!(!d.contains("QI") && !d.contains("ZA"));
        assert!(!d.contains("A") && !d.contains("ZZZZ"));
        assert_eq!(d.longest_playable(&rack_of("TSERNIA"), 2).map(str::len), Some(7));
        assert_eq!(d.longest_playable(&rack_of("Q"), 2), None);
        assert_eq!(d.longest_playable(&rack_of("QI"), 2), None);
        assert_eq!(d.longest_playable(&rack_of("QAT"), 2), Some("QAT"));
        assert_eq!(d.longest_playable(&rack_of("QAT"), 4), None);
        let w = d.longest_playable(&rack_of("AEXZ"), 2).unwrap();
        assert!(can_spell(w, &rack_of("AEXZ")) && d.contains(w));
    }
}
