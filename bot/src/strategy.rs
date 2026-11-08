//! The real strategy.
//!
//! Bidding: a tile's worth to us is its *marginal* value in points: how much
//! the best set of words we can eventually cash out grows when the tile joins
//! our rack, averaged over Monte Carlo draws of the tiles we're likely to win
//! later in the match (common random numbers for both arms). Points convert to
//! coins at a rate that paces our budget (coins are only worth what they can
//! buy before the bag empties; leftover balance is worthless).
//!
//! Words: hold tiles (racks are unlimited and private), cash 7+ letter words
//! (3x, the cap) as soon as they appear, cash shorter words only when we need
//! coins or the match is ending, and dump the best disjoint word set when the
//! last auction opens (its tile can never be played).

use std::collections::{HashMap, HashSet};

use crate::engine::{self, Engine, R8, BAG};
use crate::strategy_stub::{
    AuctionKind, AuctionOutcome, BidContext, MatchInfo, Participant, Strategy, WordContext,
};

#[derive(Clone, Debug)]
pub struct Params {
    /// Coins per marginal point at the reference balance.
    pub r0: f64,
    /// Pacing exponent on balance / bref.
    pub gamma: f64,
    pub bref: f64,
    /// Expected fraction of the remaining usable auctions we win.
    pub p_win: f64,
    pub kmax: usize,
    pub samples: usize,
    /// When at most this many usable auctions remain, multiply the rate by end_mult.
    pub end_rounds: u32,
    pub end_mult: f64,
    /// Below this balance, cash 5+ letter words.
    pub low_bal: i64,
    /// Below this balance, cash 6+ letter words.
    pub mid_bal: i64,
    /// When at most this many usable auctions remain, cash 6-letter words.
    pub play6_rounds: u32,
    /// Weight of the leave value when choosing which word to cash.
    pub leave_w: f64,
    /// Floor bid (coins) for any tile with positive marginal value.
    pub min_bid: f64,
    pub cands: usize,
    pub nodes: u32,
    /// First-price: scale on the Vickrey-era competing-bid table.
    pub fp_scale: f64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            r0: 2.5,
            gamma: 0.5,
            bref: 100.0,
            p_win: 0.2,
            kmax: 10,
            samples: 48,
            end_rounds: 10,
            end_mult: 3.0,
            low_bal: 12,
            mid_bal: 25,
            play6_rounds: 10,
            leave_w: 0.5,
            min_bid: 0.0,
            cands: 160,
            nodes: 2500,
            fp_scale: 0.85,
        }
    }
}

impl Params {
    /// Overrides like "r0=5,gamma=0.7".
    pub fn apply(&mut self, spec: &str) -> Result<(), String> {
        for kv in spec.split([',', ' ']).filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').ok_or(format!("bad param {kv}"))?;
            let f: f64 = v.parse().map_err(|e| format!("{kv}: {e}"))?;
            match k {
                "r0" => self.r0 = f,
                "gamma" => self.gamma = f,
                "bref" => self.bref = f,
                "p_win" => self.p_win = f,
                "kmax" => self.kmax = f as usize,
                "samples" => self.samples = f as usize,
                "end_rounds" => self.end_rounds = f as u32,
                "end_mult" => self.end_mult = f,
                "low_bal" => self.low_bal = f as i64,
                "mid_bal" => self.mid_bal = f as i64,
                "play6_rounds" => self.play6_rounds = f as u32,
                "leave_w" => self.leave_w = f,
                "min_bid" => self.min_bid = f,
                "cands" => self.cands = f as usize,
                "nodes" => self.nodes = f as u32,
                "fp_scale" => self.fp_scale = f,
                _ => return Err(format!("unknown param {k}")),
            }
        }
        Ok(())
    }
}

/// Competing top-bid samples by (letter, phase) from live 6-real-bot matches.
pub struct PriceTable {
    t: Vec<Vec<Vec<u16>>>,
}

pub fn phase_of(round: u32) -> usize {
    if round >= 68 {
        7
    } else if round >= 61 {
        6
    } else {
        ((round.max(1) - 1) / 10) as usize
    }
}

impl PriceTable {
    pub fn load() -> PriceTable {
        let mut t = vec![vec![Vec::new(); 8]; 26];
        for line in include_str!("../../data/prices_r6.txt").lines() {
            let mut it = line.split_whitespace();
            let (Some(l), Some(p)) = (it.next(), it.next()) else { continue };
            let Some(li) = l.chars().next().and_then(engine::idx) else { continue };
            let Ok(p) = p.parse::<usize>() else { continue };
            if p >= 8 {
                continue;
            }
            let mut v: Vec<u16> = it.filter_map(|x| x.parse().ok()).collect();
            v.sort_unstable();
            t[li][p] = v;
        }
        // Fill sparse cells from neighbours.
        for li in 0..26 {
            for p in 0..8 {
                if t[li][p].len() < 5 {
                    let mut merged: Vec<u16> = (0..7).flat_map(|q| t[li][q].clone()).collect();
                    merged.sort_unstable();
                    if merged.is_empty() {
                        merged = vec![10];
                    }
                    t[li][p] = merged;
                }
            }
        }
        PriceTable { t }
    }

    pub fn samples(&self, letter: usize, round: u32) -> &[u16] {
        &self.t[letter][phase_of(round)]
    }

    /// P(competing top bid < b), ties count half.
    pub fn p_beat(&self, letter: usize, round: u32, b: f64, scale: f64) -> f64 {
        let s = self.samples(letter, round);
        if s.is_empty() {
            return 0.5;
        }
        let mut lt = 0usize;
        let mut eq = 0usize;
        for &x in s {
            let x = (x as f64 * scale).round();
            if x < b {
                lt += 1;
            } else if x == b {
                eq += 1;
            }
        }
        (lt as f64 + 0.5 * eq as f64) / s.len() as f64
    }
}

#[derive(Default)]
struct MatchMem {
    seen: R8,
    seen_ids: HashSet<u64>,
    start_rack: Option<R8>,
    kind: Option<AuctionKind>,
}

pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15 | 1)
    }
    pub fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    pub fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
    pub fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

pub struct SmartStrategy {
    pub eng: &'static Engine,
    pub prices: &'static PriceTable,
    pub p: Params,
    rng: Rng,
    mem: HashMap<u64, MatchMem>,
    pub note: String,
}

pub fn to_r8(r: &[u32; 26]) -> R8 {
    let mut o = [0u8; 26];
    for i in 0..26 {
        o[i] = r[i].min(255) as u8;
    }
    o
}

impl SmartStrategy {
    pub fn new(eng: &'static Engine, prices: &'static PriceTable, p: Params, seed: u64) -> Self {
        SmartStrategy { eng, prices, p, rng: Rng::new(seed), mem: HashMap::new(), note: String::new() }
    }

    fn value(&self, r: &R8) -> u32 {
        self.eng.value(r, self.p.cands, self.p.nodes)
    }

    /// Estimated letters still to come (unknown pool): bag minus seen auction
    /// tiles minus our own starting rack. Opponents' hidden starting tiles stay
    /// in the pool (unknowable).
    fn pool(&self, match_id: u64) -> Vec<u8> {
        let mut counts = BAG;
        if let Some(m) = self.mem.get(&match_id) {
            for i in 0..26 {
                counts[i] = counts[i].saturating_sub(m.seen[i]);
                if let Some(s) = &m.start_rack {
                    counts[i] = counts[i].saturating_sub(s[i]);
                }
            }
        }
        let mut v = Vec::new();
        for (i, &c) in counts.iter().enumerate() {
            for _ in 0..c {
                v.push(i as u8);
            }
        }
        v
    }

    /// Marginal points of adding tile `l` to `rack`, with `k` future tiles.
    pub fn marginal(&mut self, rack: &R8, l: usize, pool: &[u8], k: usize) -> f64 {
        let k = k.min(pool.len());
        let n = if k == 0 { 1 } else { self.p.samples.max(1) };
        let mut pool = pool.to_vec();
        let mut tot = 0.0;
        for _ in 0..n {
            let mut base = *rack;
            // Partial Fisher-Yates: first k entries become the sample.
            for j in 0..k {
                let r = j + self.rng.below(pool.len() - j);
                pool.swap(j, r);
                base[pool[j] as usize] += 1;
            }
            let without = self.value(&base);
            base[l] += 1;
            let with = self.value(&base);
            tot += with as f64 - without as f64;
        }
        tot / n as f64
    }

    fn mem(&mut self, match_id: u64) -> &mut MatchMem {
        self.mem.entry(match_id).or_default()
    }

    /// Choose a word to cash now among words of length >= min_len, scoring
    /// reward + leave_w * value(rest).
    fn pick_word(&self, rack: &R8, min_len: usize) -> Option<u32> {
        let mut c = Vec::new();
        self.eng.playable(rack, min_len, 40, &mut c);
        let mut best: Option<(f64, u32)> = None;
        for &wi in c.iter().take(25) {
            let w = self.eng.word(wi);
            let mut rest = *rack;
            engine::sub(&mut rest, &w.counts);
            let s = w.reward as f64 + self.p.leave_w * self.value(&rest) as f64;
            if best.is_none_or(|(b, _)| s > b) {
                best = Some((s, wi));
            }
        }
        best.map(|(_, wi)| wi)
    }

    pub fn bid_amount(&mut self, c: &BidContext) -> i64 {
        let Some(l) = engine::idx(c.letter) else { return 0 };
        let rack = to_r8(&c.rack);
        {
            let mem = self.mem(c.match_id);
            if mem.start_rack.is_none() {
                mem.start_rack = Some(rack);
            }
            if mem.seen_ids.insert(c.auction_id) {
                mem.seen[l] += 1;
            }
            mem.kind = Some(c.auction_kind);
        }
        if c.bag_remaining == 0 {
            self.note = "last auction: tile is unplayable".into();
            return 0;
        }
        // Auctions after this one whose tiles can still be played.
        let useful_future = c.bag_remaining.saturating_sub(1);
        let k = ((useful_future as f64 * self.p.p_win).round() as usize).min(self.p.kmax);
        let pool = self.pool(c.match_id);
        let marg = self.marginal(&rack, l, &pool, k);
        let bal = c.my_balance.max(0) as f64;
        let mut rate = self.p.r0 * (bal.max(1.0) / self.p.bref).powf(self.p.gamma);
        if useful_future <= self.p.end_rounds {
            rate *= self.p.end_mult;
        }
        let val = if marg > 0.0 { (marg * rate).max(self.p.min_bid) } else { 0.0 };
        let bid = match c.auction_kind {
            AuctionKind::Vickrey => val,
            AuctionKind::FirstPrice => self.shade(val, l, c.round),
        };
        let out = (bid.round() as i64).clamp(0, c.my_balance.max(0));
        self.note = format!("marg={marg:.2} k={k} rate={rate:.2} val={val:.1}");
        out
    }

    /// First-price: maximise P(win) * (value - bid) against the competing-bid table.
    fn shade(&self, val: f64, l: usize, round: u32) -> f64 {
        if val <= 1.0 {
            return val;
        }
        let mut best = (0.0, 0.0);
        let mut b = 1.0;
        while b <= val {
            let e = self.prices.p_beat(l, round, b, self.p.fp_scale) * (val - b);
            if e > best.0 {
                best = (e, b);
            }
            b += 1.0;
        }
        best.1
    }

    pub fn words_now(&mut self, c: &WordContext) -> Vec<String> {
        let rack = to_r8(&c.rack);
        if engine::rack_total(&rack) < 2 {
            return vec![];
        }
        if c.bag_remaining == 0 {
            // Last auction is open: cash out everything we can.
            let (_, set) = self.eng.best_set(&rack, 400, 300_000);
            return set.iter().map(|&i| self.eng.word(i).text.clone()).collect();
        }
        let useful_future = c.bag_remaining.saturating_sub(1);
        if let Some(wi) = self.pick_word(&rack, 7) {
            return vec![self.eng.word(wi).text.clone()];
        }
        let min_len = if c.my_balance < self.p.low_bal {
            5
        } else if c.my_balance < self.p.mid_bal || useful_future <= self.p.play6_rounds {
            6
        } else {
            return vec![];
        };
        if let Some(wi) = self.pick_word(&rack, min_len) {
            return vec![self.eng.word(wi).text.clone()];
        }
        vec![]
    }
}

impl Strategy for SmartStrategy {
    fn name(&self) -> String {
        format!("smart{:?}", self.p)
    }

    fn on_match_start(&mut self, m: &MatchInfo) {
        self.mem(m.match_id).kind = Some(m.auction_kind);
        // Hot-reload parameter overrides between matches (no restart needed).
        if let Ok(path) = std::env::var("ZC_PARAMS_FILE") {
            if let Ok(spec) = std::fs::read_to_string(&path) {
                let mut p = Params::default();
                if let Ok(env) = std::env::var("ZC_PARAMS") {
                    let _ = p.apply(&env);
                }
                match p.apply(spec.trim()) {
                    Ok(()) => self.p = p,
                    Err(e) => eprintln!("ZC_PARAMS_FILE {path}: {e}"),
                }
            }
        }
    }

    fn decide_bid(&mut self, ctx: &BidContext) -> i64 {
        self.bid_amount(ctx)
    }

    fn choose_word(&mut self, ctx: &WordContext) -> Option<String> {
        self.words_now(ctx).into_iter().next()
    }

    fn choose_words(&mut self, ctx: &WordContext) -> Vec<String> {
        self.words_now(ctx)
    }

    fn on_auction_result(&mut self, r: &AuctionOutcome) {
        if let Some(l) = engine::idx(r.letter) {
            let mem = self.mem(r.match_id);
            if mem.seen_ids.insert(r.auction_id) {
                mem.seen[l] += 1;
            }
        }
    }

    fn on_match_end(&mut self, match_id: u64, _standings: &[Participant]) {
        self.mem.remove(&match_id);
    }

    fn note(&self) -> String {
        self.note.clone()
    }
}
