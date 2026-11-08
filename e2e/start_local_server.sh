#!/usr/bin/env bash
# Start a local SpacetimeDB standalone server, detached so it outlives the
# calling shell. Does nothing if one is already answering on $STDB_PORT.
set -euo pipefail
source "$(cd "$(dirname "$0")" && pwd)/env.sh"

ping_ok() { curl -fsS -m 2 "$STDB_LOCAL_URL/v1/ping" >/dev/null 2>&1; }

if ping_ok; then
  echo "SpacetimeDB already running at $STDB_LOCAL_URL (log: $STDB_LOG)"
  exit 0
fi
if lsof -iTCP:"$STDB_PORT" -sTCP:LISTEN -n -P >/dev/null 2>&1; then
  echo "port $STDB_PORT is held by something that isn't SpacetimeDB:" >&2
  lsof -iTCP:"$STDB_PORT" -sTCP:LISTEN -n -P >&2
  exit 1
fi

mkdir -p "$STDB_DATA_DIR"
# macOS has no setsid(1): fork, then POSIX::setsid in the child, then exec.
# The server gets its own session and survives this shell; nohup + </dev/null
# keep SIGHUP and the terminal away from it.
nohup perl -MPOSIX -e 'fork and exit 0; POSIX::setsid(); exec @ARGV or die "exec: $!"' \
  spacetime start --listen-addr "127.0.0.1:${STDB_PORT}" --data-dir "$STDB_DATA_DIR" --non-interactive \
  >>"$STDB_LOG" 2>&1 </dev/null &

for _ in $(seq 1 120); do
  if ping_ok; then
    echo "SpacetimeDB started at $STDB_LOCAL_URL (data: $STDB_DATA_DIR, log: $STDB_LOG)"
    exit 0
  fi
  sleep 0.5
done
echo "SpacetimeDB did not come up on $STDB_LOCAL_URL; last log lines:" >&2
tail -30 "$STDB_LOG" >&2
exit 1
