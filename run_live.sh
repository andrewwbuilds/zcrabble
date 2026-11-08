#!/usr/bin/env bash
# Run zcrabble-bot against the live ScrabbleBot database (maincloud).
#
#   BOT_TOKEN=<token from the team page> ./run_live.sh   # first run: saves it to .token
#   ./run_live.sh                                          # later runs
#
# The bot joins lobbies forever, re-joins after each match, reconnects on
# disconnect, and auto-registers for any tournament open for registration
# (AUTO_TOURNAMENT=0 to disable). Tune with ZC_PARAMS="r0=2.5,gamma=0.5,...".
set -euo pipefail
cd "$(dirname "$0")"
TOKEN_FILE="${BOT_TOKEN_FILE:-.token}"
if [[ -n "${BOT_TOKEN:-}" ]]; then
  umask 077
  printf '%s\n' "$BOT_TOKEN" >"$TOKEN_FILE"
fi
if [[ ! -s "$TOKEN_FILE" ]]; then
  echo "No token: run once with BOT_TOKEN=<token from https://scrabblebot.spacetimedb.com/team>" >&2
  exit 2
fi
cargo build --release -q -p zcrabble-bot
unset BOT_TOKEN
export BOT_TOKEN_FILE="$TOKEN_FILE"
exec ./target/release/zcrabble-bot
