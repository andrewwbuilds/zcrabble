#!/usr/bin/env bash
# Run N zcrabble-bot processes (zc1..zcN, separate tokens) against the local
# server until each has finished MATCHES matches, then print the final scores
# of every match they played.
#
#   e2e/run_local_match.sh [N=4] [MATCHES=1]
#
# N=6 fills the lobby with real bots, so the match starts at once; fewer wait
# for the lobby timer (90 s, counted from when that lobby opened) and are
# padded with sims. MATCHES>1 shows the bots re-joining after a match ends.
# Bot env vars (BOT_VERBOSE, BOT_MIN_WORD_LEN, BOT_SUBSCRIBE, ...) pass through.
# Logs: e2e/logs/run-<timestamp>/<bot>.log. Exit 0 iff every bot exits 0 and
# every match it played ended.
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

N="${1:-4}"
MATCHES="${2:-1}"
read -r -a ALL_BOTS <<<"$ZC_BOTS"
if ! [[ "$N" =~ ^[0-9]+$ && "$N" -ge 1 && "$N" -le ${#ALL_BOTS[@]} ]]; then
  echo "N must be 1..${#ALL_BOTS[@]} (bots: $ZC_BOTS)" >&2
  exit 2
fi
if ! [[ "$MATCHES" =~ ^[0-9]+$ && "$MATCHES" -ge 1 ]]; then
  echo "MATCHES must be >= 1" >&2
  exit 2
fi
BOTS=("${ALL_BOTS[@]:0:$N}")
RUN_DIR="$E2E_DIR/logs/run-$(date +%Y%m%d-%H%M%S)"
mkdir -p "$RUN_DIR"

if ! "$E2E_DIR/setup_local.sh" >"$RUN_DIR/setup.log" 2>&1; then
  cat "$RUN_DIR/setup.log" >&2
  exit 1
fi
if ! (cd "$ZC_ROOT" && cargo build --release -q -p zcrabble-bot) >"$RUN_DIR/build.log" 2>&1; then
  cat "$RUN_DIR/build.log" >&2
  exit 1
fi
BIN="$ZC_ROOT/target/release/zcrabble-bot"

PIDS=()
cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
}
trap cleanup EXIT
trap 'exit 130' INT TERM

echo "launching ${BOTS[*]} against $STDB_LOCAL_URL ($MATCHES match(es) each); logs in $RUN_DIR"
for b in "${BOTS[@]}"; do
  STDB_HOST="$STDB_LOCAL_URL" STDB_DB="$STDB_DB_NAME" BOT_TOKEN_FILE="$TOKENS_DIR/$b.token" \
    BOT_NAME="$b" BOT_MAX_MATCHES="$MATCHES" "$BIN" >"$RUN_DIR/$b.log" 2>&1 &
  PIDS+=("$!")
done

# Lobby wait (<= 90 s) + ~70 s per match + slack.
START=$(date +%s)
DEADLINE=$((START + 150 + MATCHES * 170))
LAST_REPORT=$START
while :; do
  alive=0
  for pid in "${PIDS[@]}"; do kill -0 "$pid" 2>/dev/null && alive=$((alive + 1)); done
  [[ $alive -eq 0 ]] && break
  now=$(date +%s)
  if ((now >= DEADLINE)); then
    echo "TIMEOUT after $((now - START))s with $alive bot(s) still running" >&2
    break
  fi
  if ((now - LAST_REPORT >= 15)); then
    LAST_REPORT=$now
    last=$(grep -hE 'LOBBY_IN|MATCH_START|MATCH_END|BID_OK' "$RUN_DIR/${BOTS[0]}.log" | tail -1 | cut -c1-140 || true)
    echo "  t=$((now - START))s alive=$alive  ${BOTS[0]}: $last"
  fi
  sleep 1
done

FAILED=0
for i in "${!BOTS[@]}"; do
  if kill -0 "${PIDS[$i]}" 2>/dev/null; then
    kill "${PIDS[$i]}" 2>/dev/null || true
    echo "${BOTS[$i]}: killed (still running at deadline)"
    FAILED=1
  elif wait "${PIDS[$i]}"; then
    :
  else
    echo "${BOTS[$i]}: exited with status $?"
    FAILED=1
  fi
done
PIDS=()

echo
echo "== per-bot event counts"
printf '%-6s %6s %6s %8s %5s %6s %8s %7s %6s %s\n' bot bids bid_ok rejected won words wordrej rejoins ended matches
for b in "${BOTS[@]}"; do
  f="$RUN_DIR/$b.log"
  c() { grep -c -E "$1" "$f" || true; }
  matches=$(grep -oE 'MATCH_END match=[0-9]+' "$f" | cut -d= -f2 | tr '\n' ' ')
  printf '%-6s %6s %6s %8s %5s %6s %8s %7s %6s %s\n' "$b" "$(c '\] BID auction')" "$(c 'BID_OK')" \
    "$(c 'BID_REJECTED|BID_ERROR')" "$(c '\] WON ')" "$(c 'PLAY_OK')" "$(c 'PLAY_REJECTED|PLAY_ERROR')" \
    "$(c 'LOBBY_JOIN reason=match ended')" "$(c 'MATCH_END')" "$matches"
done

echo
echo "== ${BOTS[0]} log excerpt"
grep -E 'CONNECTED|BOT id|LOBBY_IN|MATCH_START|MATCH_END|MATCH_SUMMARY|LATENCY|LOBBY_JOIN reason=match|DONE|EXIT|FATAL|RECONNECT' \
  "$RUN_DIR/${BOTS[0]}.log" | cut -c1-260 || true
grep -m3 -E '\] BID auction' "$RUN_DIR/${BOTS[0]}.log" | cut -c1-200 || true
grep -m3 -E 'BID_OK' "$RUN_DIR/${BOTS[0]}.log" | cut -c1-200 || true
grep -m2 -E '\] WON |PLAY_OK' "$RUN_DIR/${BOTS[0]}.log" | cut -c1-200 || true

MATCH_IDS=$(grep -hoE 'MATCH_END match=[0-9]+' "$RUN_DIR"/*.log | cut -d= -f2 | sort -nu | tr '\n' ' ')
echo
if [[ -z "$MATCH_IDS" ]]; then
  echo "no match finished" >&2
  exit 1
fi
echo "== final scores (SQL on the local database)"
# shellcheck disable=SC2086
python3 "$E2E_DIR/stdb_local.py" report $MATCH_IDS || FAILED=1
echo
echo "== bots' final standings (client cache) vs database"
python3 "$E2E_DIR/stdb_local.py" check-logs "$RUN_DIR" || FAILED=1
echo
echo "logs: $RUN_DIR"
exit $FAILED
