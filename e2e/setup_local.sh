#!/usr/bin/env bash
# Local ScrabbleBot e2e environment, idempotent: safe to run any number of times.
#
#   e2e/setup_local.sh              start the server if needed, publish the module if the
#                                   database doesn't exist, make the owner an admin, check the
#                                   sims, provision test bots ($ZC_BOTS) and their tokens
#   e2e/setup_local.sh --republish  also publish the current module source (keeps data)
#   e2e/setup_local.sh --reset      wipe the database (publish --delete-data=always) first;
#                                   bot token files stay valid and are re-claimed
#
# Tokens: e2e/tokens/<bot>.token (bot credential, give it to zcrabble-bot as BOT_TOKEN_FILE)
#         e2e/tokens/<bot>.human.token (team owner; mints more credentials)
# The local database owner/admin token lives in e2e/.stdb/cli.toml; use it with
#   source e2e/env.sh && stdb sql scrabblebot "SELECT * FROM bot"
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

RESET=0
REPUBLISH=0
for arg in "$@"; do
  case "$arg" in
    --reset) RESET=1 ;;
    --republish) REPUBLISH=1 ;;
    -h|--help) sed -n '2,15p' "$0"; exit 0 ;;
    *) echo "unknown argument: $arg" >&2; exit 2 ;;
  esac
done
PY=(python3 "$E2E_DIR/stdb_local.py")

"$E2E_DIR/start_local_server.sh"
"${PY[@]}" ensure-owner
OWNER_ID=$(python3 -c 'import sys,re,json,base64; t=re.search(r"spacetimedb_token\s*=\s*\"([^\"]+)\"", open(sys.argv[1]).read()).group(1).split(".")[1]; print(json.loads(base64.urlsafe_b64decode(t+"="*(-len(t)%4)))["hex_identity"])' "$STDB_CLI_CONFIG")

if [[ ! -d "$SCRABBLEBOT_MODULE/node_modules" ]]; then
  echo "installing module dependencies (npm ci) in $SCRABBLEBOT_MODULE"
  (cd "$SCRABBLEBOT_MODULE" && npm ci --no-audit --no-fund)
fi

DB_OWNER=$("${PY[@]}" db-owner)
if [[ -n "$DB_OWNER" && "$DB_OWNER" != "$OWNER_ID" ]]; then
  echo "database '$STDB_DB_NAME' is owned by $DB_OWNER, not this e2e owner ($OWNER_ID)." >&2
  echo "Stop the server, delete $STDB_DATA_DIR, and rerun." >&2
  exit 1
fi
# Never let spacetime.json in the module repo (server: maincloud) steer this.
PUBLISH=(stdb publish --no-config --server local -p "$SCRABBLEBOT_MODULE" -y)
if [[ $RESET == 1 ]]; then
  echo "publishing $STDB_DB_NAME with --delete-data=always"
  "${PUBLISH[@]}" --delete-data=always "$STDB_DB_NAME" 2>&1 | grep -vE 'UNSTABLE|^\s*$' | tail -3
elif [[ -z "$DB_OWNER" || $REPUBLISH == 1 ]]; then
  echo "publishing $STDB_DB_NAME"
  "${PUBLISH[@]}" "$STDB_DB_NAME" 2>&1 | grep -vE 'UNSTABLE|^\s*$' | tail -3
else
  echo "database $STDB_DB_NAME exists (owner $OWNER_ID); not republishing"
fi

"${PY[@]}" ensure-admin
"${PY[@]}" ensure-sims
# shellcheck disable=SC2086
"${PY[@]}" ensure-bots $ZC_BOTS
echo
echo "local server: $STDB_LOCAL_URL  database: $STDB_DB_NAME  log: $STDB_LOG"
echo "run a bot:    STDB_HOST=$STDB_LOCAL_URL BOT_TOKEN_FILE=$TOKENS_DIR/zc1.token $ZC_ROOT/target/release/zcrabble-bot"
