# zcrabble

A Rust bot for [ScrabbleBot](https://scrabblebot.spacetimedb.com/), the Scrabble-style
sealed-bid tile auction game built on [SpacetimeDB](https://spacetimedb.com/).

## How it plays

- **Valuation by Monte Carlo.** A tile is worth the *marginal* points it adds to the
  best set of disjoint dictionary words our rack can eventually be cashed out for.
  We estimate it by sampling the tiles we're likely to win later (from the bag
  minus every tile seen so far), with common random numbers for the with/without
  arms. A tile that completes a 7-letter word is worth far more than a duplicate
  vowel, and the bot bids accordingly.
- **Budget pacing.** Points convert to coins at a rate that scales with the square
  root of the balance. The rate rises in the last auctions, because coins left at
  the end are worth nothing.
- **Auction-aware.** Vickrey (lobby default): bid true value. First-price
  (tournaments may use it): shade to maximise `P(win) * (value - bid)` against the
  competing-bid distribution measured from about 78k live auctions.
- **Word timing.** Racks are private and unlimited, so the bot holds tiles. It
  cashes 7+ letter words (the 3x cap) as soon as they appear, and cashes 5–6
  letter words only when it needs coins or the match is ending. When the last
  auction opens (its tile can never be played), it plays the best disjoint
  word set all at once.
- **Never misses the last second.** The bot skips the final auction (its tile is
  dead), and it triggers the cash-out on the auction insert as well as on rack
  changes.
- **Tournaments.** It registers itself for any tournament that opens for
  registration.

## Layout

- `bot/src/main.rs`: SpacetimeDB client (Rust SDK 2.14.1). It uses scoped
  subscriptions, reconnects, and joins the lobby automatically.
- `bot/src/engine.rs`: word engine (playable words, best disjoint word set via
  branch-and-bound).
- `bot/src/strategy.rs`: bidding and word strategy.
- `bot/src/bin/sim.rs`: offline match simulator (price-taker model using live
  price data in `data/`), used to tune parameters.
- `e2e/`: local end-to-end harness (local SpacetimeDB, module publish, test bots).

## Run

```sh
cargo build --release
BOT_TOKEN=<token from the team page> ./run_live.sh   # first run saves .token
./run_live.sh                                         # afterwards
```

Simulate: `./target/release/sim 400 r0=2.5 gamma=0.5`.

Local end-to-end: `./e2e/setup_local.sh && ./e2e/run_local_match.sh 6 1`.
