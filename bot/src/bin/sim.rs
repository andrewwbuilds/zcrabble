//! Offline match simulator (price-taker model).
//!
//! Our strategy plays a full 6-seat match (5 starting tiles each, 98-tile bag,
//! 68 one-tile auctions). The other five seats are summarised by the highest
//! competing bid per auction, drawn from live 6-real-bot matches for the same
//! letter and phase (data/prices_r6.txt). Vickrey: we pay that bid if we beat
//! it. Final score is compared with the best opponent score from live matches
//! (data/oppmax_r6.txt) to estimate win probability.
//!
//! Usage: sim <matches> [param overrides like r0=5,gamma=0.5] [--fp]

#![allow(dead_code)]

#[path = "../dict.rs"]
mod dict;
#[path = "../engine.rs"]
mod engine;
#[path = "../strategy.rs"]
mod strategy;
#[path = "../strategy_stub.rs"]
mod strategy_stub;

use engine::{Engine, BAG};
use strategy::{Params, PriceTable, Rng, SmartStrategy};
use strategy_stub::{AuctionKind, AuctionOutcome, BidContext, MatchInfo, Strategy, WordContext};

struct Res {
    score: i64,
    won: u32,
    spent: i64,
    left_bal: i64,
    left_tiles: usize,
    words: u32,
    win: f64,
}

fn play(eng: &'static Engine, prices: &'static PriceTable, p: &Params, seed: u64, fp: bool, opp: &[i64]) -> Res {
    let mut rng = Rng::new(seed.wrapping_mul(0x2545F4914F6CDD1D) ^ 0xABCDEF);
    let mut strat = SmartStrategy::new(eng, prices, p.clone(), seed ^ 0x1234_5678);
    let kind = if fp { AuctionKind::FirstPrice } else { AuctionKind::Vickrey };
    let mut bag: Vec<u8> = Vec::new();
    for (i, &c) in BAG.iter().enumerate() {
        for _ in 0..c {
            bag.push(i as u8);
        }
    }
    // Shuffle.
    for i in (1..bag.len()).rev() {
        let j = rng.below(i + 1);
        bag.swap(i, j);
    }
    let mut rack = [0u32; 26];
    for _ in 0..5 {
        rack[bag.pop().unwrap() as usize] += 1;
    }
    for _ in 0..25 {
        bag.pop();
    }
    let match_id = 1;
    strat.on_match_start(&MatchInfo {
        match_id,
        my_bot_id: 1,
        auction_kind: kind,
        auction_duration_ms: 1000,
        starting_rack_size: 5,
        participants: vec![],
    });
    let (mut balance, mut score, mut spent, mut won, mut words) = (100i64, 0i64, 0i64, 0u32, 0u32);
    let mut round = 0u32;
    while let Some(l) = bag.pop() {
        round += 1;
        let bag_remaining = bag.len() as u32;
        // Words when the auction opens (rack may have changed).
        loop {
            let ws = strat.choose_words(&WordContext {
                match_id,
                rack,
                bag_remaining,
                my_balance: balance,
                my_score: score,
                participants: &[],
            });
            if ws.is_empty() {
                break;
            }
            let mut any = false;
            for w in ws {
                if dict::can_spell(&w, &rack) {
                    for c in w.chars() {
                        rack[dict::letter_index(c).unwrap()] -= 1;
                    }
                    let r = dict::word_reward(&w);
                    if std::env::var("SIMV").is_ok() { eprintln!("   PLAY {w} +{r} bag={bag_remaining}"); }
                    score += r;
                    balance += r;
                    words += 1;
                    any = true;
                }
            }
            if !any {
                break;
            }
        }
        let bid = strat
            .decide_bid(&BidContext {
                match_id,
                auction_id: round as u64,
                letter: engine::ch(l as usize),
                round,
                bag_remaining,
                auction_kind: kind,
                my_balance: balance,
                my_score: score,
                rack,
                participants: &[],
                opens_at_micros: 0,
                closes_at_micros: 0,
                now_micros: 0,
            })
            .clamp(0, balance);
        let comp = comp_bid(l as usize, round, &mut rng);
        let comp = if fp { (comp as f64 * p.fp_scale).round() as i64 } else { comp };
        let win = bid > 0 && (bid > comp || (bid == comp && rng.unit() < 0.5));
        if std::env::var("SIMV").is_ok() {
            eprintln!("r{round:02} bag={bag_remaining:02} {} bid={bid:3} comp={comp:3} win={} bal={balance:3} score={score:3} rack={} {}", engine::ch(l as usize), win as u8, engine::rack_str(&strategy::to_r8(&rack)), strat.note);
        }
        if win {
            let pay = if fp { bid } else { comp.max(1) };
            balance -= pay;
            spent += pay;
            rack[l as usize] += 1;
            won += 1;
        }
        strat.on_auction_result(&AuctionOutcome {
            match_id,
            auction_id: round as u64,
            letter: engine::ch(l as usize),
            winner_bot_id: if win { Some(1) } else { Some(2) },
            paid: 0,
            mine: win,
        });
    }
    let left_tiles = rack.iter().sum::<u32>() as usize;
    let o = opp[rng.below(opp.len())];
    let winp = if score > o { 1.0 } else if score == o { 0.5 } else { 0.0 };
    Res { score, won, spent, left_bal: balance, left_tiles, words, win: winp }
}

static PAIRS: std::sync::OnceLock<Vec<Vec<Vec<(u16, u16)>>>> = std::sync::OnceLock::new();

/// Highest bid among the other five seats: a random live auction of this
/// letter and phase; with prob 1/6 we took the top bidder's seat, so the
/// competition is that auction's runner-up (its `paid`).
fn comp_bid(l: usize, round: u32, rng: &mut Rng) -> i64 {
    let t = PAIRS.get_or_init(|| {
        let mut t = vec![vec![Vec::new(); 8]; 26];
        let src = if std::env::var("SIM_FRESH").is_ok() {
            include_str!("../../../data/prices2_fresh.txt")
        } else {
            include_str!("../../../data/prices2_r6.txt")
        };
        for line in src.lines() {
            let mut it = line.split_whitespace();
            let li = engine::idx(it.next().unwrap().chars().next().unwrap()).unwrap();
            let p: usize = it.next().unwrap().parse().unwrap();
            t[li][p] = it
                .filter_map(|x| x.split_once(':'))
                .map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap()))
                .collect();
        }
        for li in 0..26 {
            for p in 0..8 {
                if t[li][p].len() < 5 {
                    t[li][p] = (0..7).flat_map(|q| t[li][q].clone()).collect();
                }
            }
        }
        t
    });
    let cell = &t[l][strategy::phase_of(round)];
    if cell.is_empty() {
        return 10;
    }
    let (top, paid) = cell[rng.below(cell.len())];
    if std::env::var("SIM_TOPONLY").is_ok() || rng.below(6) != 0 {
        top as i64
    } else {
        paid as i64
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(200);
    let mut p = Params::default();
    let mut fp = false;
    let mut seed0 = 1u64;
    for a in &args[2..] {
        if a == "--fp" {
            fp = true;
        } else if let Some(s) = a.strip_prefix("seed=") {
            seed0 = s.parse().unwrap();
        } else {
            p.apply(a).unwrap();
        }
    }
    let eng: &'static Engine = Box::leak(Box::new(Engine::load()));
    let prices: &'static PriceTable = Box::leak(Box::new(PriceTable::load()));
    let opp: &'static Vec<i64> = Box::leak(Box::new(
        (if std::env::var("SIM_FRESH").is_ok() {
            include_str!("../../../data/oppmax_fresh.txt")
        } else {
            include_str!("../../../data/oppmax_r6.txt")
        })
        .split_whitespace()
            .filter_map(|x| x.parse().ok())
            .collect(),
    ));
    let threads = std::env::var("SIM_THREADS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|x| x.get()).unwrap_or(8).min(16));
    let t0 = std::time::Instant::now();
    let mut hs = Vec::new();
    for t in 0..threads {
        let p = p.clone();
        hs.push(std::thread::spawn(move || {
            let mut out = Vec::new();
            let mut i = t;
            while i < n {
                out.push(play(eng, prices, &p, seed0 * 1_000_003 + i as u64, fp, opp));
                i += threads;
            }
            out
        }));
    }
    let res: Vec<Res> = hs.into_iter().flat_map(|h| h.join().unwrap()).collect();
    let m = res.len() as f64;
    let mut sc: Vec<i64> = res.iter().map(|r| r.score).collect();
    sc.sort_unstable();
    let mean = |f: &dyn Fn(&Res) -> f64| res.iter().map(f).sum::<f64>() / m;
    let sd = {
        let mu = mean(&|r| r.score as f64);
        (res.iter().map(|r| (r.score as f64 - mu).powi(2)).sum::<f64>() / m).sqrt() / m.sqrt()
    };
    println!(
        "n={} score={:.1}±{:.1} p10={} p50={} p90={} win={:.3} won={:.1} spent={:.0} left_bal={:.0} left_tiles={:.1} words={:.1} secs={:.1} params={}",
        res.len(),
        mean(&|r| r.score as f64),
        sd,
        sc[sc.len() / 10],
        sc[sc.len() / 2],
        sc[sc.len() * 9 / 10],
        mean(&|r| r.win),
        mean(&|r| r.won as f64),
        mean(&|r| r.spent as f64),
        mean(&|r| r.left_bal as f64),
        mean(&|r| r.left_tiles as f64),
        mean(&|r| r.words as f64),
        t0.elapsed().as_secs_f64(),
        args[2..].join(" ")
    );
}
