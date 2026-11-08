//! Strategy hooks. `main.rs` owns the connection and calls a `Strategy` with
//! plain data, so a strategy needs nothing from SpacetimeDB.
//!
//! To plug in the real strategy (`zcrabble-core`): add it as a dependency of
//! this crate, write an adapter `impl Strategy for CoreStrategy { .. }` that
//! forwards to it, and return it from `make_strategy`. `StubStrategy` is only a
//! placeholder: face value + 1 on every tile, play the longest word as soon as
//! one exists.

// The context structs carry everything a real strategy may want; the stub
// reads only a few fields.
#![allow(dead_code)]

use crate::dict::{self, Dictionary, Rack};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuctionKind {
    /// Winner pays the runner-up's bid (or the reserve, 1, if alone).
    Vickrey,
    /// Winner pays its own bid.
    FirstPrice,
}

#[derive(Clone, Debug)]
pub struct Participant {
    pub bot_id: u64,
    pub name: String,
    pub is_sim: bool,
    pub balance: i64,
    pub score: i64,
}

#[derive(Clone, Debug)]
pub struct MatchInfo {
    pub match_id: u64,
    pub my_bot_id: u64,
    pub auction_kind: AuctionKind,
    pub auction_duration_ms: u64,
    pub starting_rack_size: u32,
    pub participants: Vec<Participant>,
}

/// Everything public (plus our own rack) when an auction opens.
#[derive(Clone, Debug)]
pub struct BidContext<'a> {
    pub match_id: u64,
    pub auction_id: u64,
    pub letter: char,
    /// 1-based auction number in the match (`match_state.current_round`).
    pub round: u32,
    /// Tiles left in the bag after this auction's tile was drawn. 0 means this
    /// is the last auction: the match ends when it closes, and no word can be
    /// played after that, so this tile can't score for a real bot.
    pub bag_remaining: u32,
    pub auction_kind: AuctionKind,
    pub my_balance: i64,
    pub my_score: i64,
    pub rack: Rack,
    pub participants: &'a [Participant],
    pub opens_at_micros: i64,
    pub closes_at_micros: i64,
    pub now_micros: i64,
}

/// Called whenever our rack in a running match changes.
#[derive(Clone, Debug)]
pub struct WordContext<'a> {
    pub match_id: u64,
    pub rack: Rack,
    /// Tiles left in the bag; 0 while the last auction is open.
    pub bag_remaining: u32,
    pub my_balance: i64,
    pub my_score: i64,
    pub participants: &'a [Participant],
}

#[derive(Clone, Debug)]
pub struct AuctionOutcome {
    pub match_id: u64,
    pub auction_id: u64,
    pub letter: char,
    /// None when nobody bid (the tile is discarded, not returned to the bag).
    pub winner_bot_id: Option<u64>,
    pub paid: i64,
    pub mine: bool,
}

#[derive(Clone, Debug)]
pub struct WordPlayed {
    pub match_id: u64,
    pub bot_id: u64,
    pub word: String,
    pub total_reward: i64,
    pub mine: bool,
}

pub trait Strategy: Send {
    fn name(&self) -> String;
    fn on_match_start(&mut self, _m: &MatchInfo) {}
    /// Amount to bid; 0 skips the auction. The caller clamps it to [0, balance].
    fn decide_bid(&mut self, ctx: &BidContext) -> i64;
    /// Word to play now from `ctx.rack` (uppercase), or None to keep the tiles.
    fn choose_word(&mut self, ctx: &WordContext) -> Option<String>;
    /// Disjoint words to play right now, sent together (e.g. the final cash-out).
    fn choose_words(&mut self, ctx: &WordContext) -> Vec<String> {
        self.choose_word(ctx).into_iter().collect()
    }
    /// Free-form explanation of the last decision, for logs.
    fn note(&self) -> String {
        String::new()
    }
    fn on_auction_result(&mut self, _r: &AuctionOutcome) {}
    fn on_word_played(&mut self, _w: &WordPlayed) {}
    fn on_match_end(&mut self, _match_id: u64, _standings: &[Participant]) {}
}

pub struct StubStrategy {
    dict: &'static Dictionary,
    min_word_len: usize,
}

impl Strategy for StubStrategy {
    fn name(&self) -> String {
        format!("stub(face+1, longest word >= {})", self.min_word_len)
    }

    fn decide_bid(&mut self, ctx: &BidContext) -> i64 {
        (dict::letter_value(ctx.letter) + 1).min(ctx.my_balance).max(0)
    }

    fn choose_word(&mut self, ctx: &WordContext) -> Option<String> {
        self.dict
            .longest_playable(&ctx.rack, self.min_word_len)
            .map(str::to_string)
    }
}

pub fn make_strategy(dict: &'static Dictionary, min_word_len: usize) -> Box<dyn Strategy> {
    if std::env::var("BOT_STRATEGY").is_ok_and(|s| s == "stub") {
        return Box::new(StubStrategy {
            dict,
            min_word_len: min_word_len.max(2),
        });
    }
    let eng: &'static crate::engine::Engine = Box::leak(Box::new(crate::engine::Engine::load()));
    let prices: &'static crate::strategy::PriceTable =
        Box::leak(Box::new(crate::strategy::PriceTable::load()));
    let mut p = crate::strategy::Params::default();
    if let Ok(spec) = std::env::var("ZC_PARAMS") {
        if let Err(e) = p.apply(&spec) {
            eprintln!("ZC_PARAMS: {e}");
            std::process::exit(2);
        }
    }
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(7);
    Box::new(crate::strategy::SmartStrategy::new(eng, prices, p, seed))
}
