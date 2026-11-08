# Shared settings for the local e2e environment. Source it, don't run it.
#   source "/Users/andrewwang/git repos/zcrabble/e2e/env.sh"
# Every value can be overridden from the environment.

E2E_DIR="$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)"
export E2E_DIR
export ZC_ROOT="${ZC_ROOT:-$(cd "$E2E_DIR/.." && pwd)}"
export PATH="$HOME/.local/bin:$HOME/.cargo/bin:$PATH"

# Port 3000 (spacetime's default) is taken on this machine, so the local server
# listens on 3300.
export STDB_PORT="${STDB_PORT:-3300}"
export STDB_LOCAL_URL="${STDB_LOCAL_URL:-http://127.0.0.1:${STDB_PORT}}"
export STDB_DATA_DIR="${STDB_DATA_DIR:-/tmp/stdb-local-data}"
export STDB_LOG="${STDB_LOG:-/tmp/stdb-local.log}"
export STDB_DB_NAME="${STDB_DB_NAME:-scrabblebot}"
export SCRABBLEBOT_MODULE="${SCRABBLEBOT_MODULE:-/tmp/scrabblebot/spacetimedb}"

# A CLI config used only for the local server, so the user's global
# ~/.config/spacetime/cli.toml (their maincloud login) is never touched. Its
# `local` server points at $STDB_LOCAL_URL and its token is the local
# database owner.
export STDB_CLI_CONFIG="${STDB_CLI_CONFIG:-$E2E_DIR/.stdb/cli.toml}"
export TOKENS_DIR="${TOKENS_DIR:-$E2E_DIR/tokens}"

# Test bot personas, one team per bot (the module allows a human on only one
# team, and a team owns exactly one bot).
ZC_BOTS_DEFAULT="zc1 zc2 zc3 zc4 zc5 zc6"
export ZC_BOTS="${ZC_BOTS:-$ZC_BOTS_DEFAULT}"

# `spacetime` against the local server with the e2e CLI config.
stdb() { spacetime --config-path "$STDB_CLI_CONFIG" "$@"; }
