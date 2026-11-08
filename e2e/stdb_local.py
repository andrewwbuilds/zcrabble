#!/usr/bin/env python3
"""Local ScrabbleBot e2e provisioning over SpacetimeDB's HTTP API (stdlib only).

Driven by setup_local.sh / run_local_match.sh, which export the settings from
env.sh. Subcommands:

  ensure-owner          make sure $STDB_CLI_CONFIG holds a valid local token
                        (the identity that publishes, so it owns the database)
  db-owner              print the database owner's identity ("" if missing)
  ensure-admin          make the owner an admin (DELETE FROM admin as the
                        owner, then bootstrap_admin: init seeds the upstream
                        admin, and bootstrap_admin only works on an empty table)
  ensure-sims           make sure the six seeded sim bots exist
  ensure-bots NAME...   one team + bot per NAME, a credential token in
                        $TOKENS_DIR/NAME.token, the team owner's token in
                        $TOKENS_DIR/NAME.human.token
  report MATCH_ID...    final standings of matches
  check-logs DIR        compare every MATCH_END standings line in DIR/*.log (the
                        bots' client caches) with the database; exit 1 on a mismatch
  status                bots, credentials, lobby, running matches

How a test bot is made (the same flow the website runs, minus spacetimedb.com):
  1. POST /v1/identity            -> a fresh "human" identity + token
  2. create_team(team, bot)       as that human: team + bot row
  3. mint_credential_nonce()      as that human; read the code from the
                                  my_nonces view with SQL as that human
  4. POST /v1/identity            -> a fresh "bot" identity + token
  5. claim_credential(code)       as the bot identity: it becomes a
                                  BotCredential; its token is the bot token
"""

import base64
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

URL = os.environ.get("STDB_LOCAL_URL", "http://127.0.0.1:3300")
DB = os.environ.get("STDB_DB_NAME", "scrabblebot")
TOKENS = pathlib.Path(os.environ.get("TOKENS_DIR", pathlib.Path(__file__).parent / "tokens"))
CLI_CONFIG = pathlib.Path(
    os.environ.get("STDB_CLI_CONFIG", pathlib.Path(__file__).parent / ".stdb" / "cli.toml")
)
PORT = URL.rsplit(":", 1)[-1].strip("/")

# SQL over HTTP returns sum types positionally: enums as [variant_index, []],
# options as [0, value] (some) / [1, []] (none).
MATCH_STATUS = ["Lobby", "Running", "Ended"]

SIMS = [
    ("Cheapo-sim", "cheapskate"),
    ("Valor-sim", "valueBidder"),
    ("Brutus-sim", "aggressive"),
    ("Hagrid-sim", "valueBidder"),
    ("Maverick-sim", "aggressive"),
    ("Snippet-sim", "cheapskate"),
]


class Fail(Exception):
    pass


def http(method, path, token=None, body=None, ctype="application/json"):
    data = body.encode() if isinstance(body, str) else body
    req = urllib.request.Request(URL + path, method=method, data=data)
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    if data is not None:
        req.add_header("Content-Type", ctype)
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except urllib.error.URLError as e:
        raise Fail(f"cannot reach {URL}: {e.reason}") from e


def new_identity():
    status, body = http("POST", "/v1/identity")
    if status != 200:
        raise Fail(f"POST /v1/identity -> {status} {body}")
    d = json.loads(body)
    return d["identity"], d["token"]


def token_identity(token):
    payload = token.split(".")[1]
    payload += "=" * (-len(payload) % 4)
    return json.loads(base64.urlsafe_b64decode(payload))["hex_identity"]


def token_valid(token):
    try:
        ident = token_identity(token)
    except Exception:
        return False
    status, _ = http("GET", f"/v1/identity/{ident}/verify", token)
    return status == 204


def call(token, reducer, *args):
    """Calls a reducer as `token`. Returns (ok, error_text)."""
    status, body = http("POST", f"/v1/database/{DB}/call/{reducer}", token, json.dumps(list(args)))
    return status == 200, body


def must_call(token, reducer, *args):
    ok, body = call(token, reducer, *args)
    if not ok:
        raise Fail(f"{reducer}{args} failed: {body.strip()}")


def _plain(v):
    # Identity/Timestamp come back as one-element products, e.g. ["0xc200.."].
    if isinstance(v, list) and len(v) == 1 and not isinstance(v[0], (list, dict)):
        return v[0]
    return v


def sql(token, query):
    status, body = http("POST", f"/v1/database/{DB}/sql", token, query, "text/plain")
    if status != 200:
        raise Fail(f"SQL failed ({status}): {query}\n  {body.strip()}")
    out = []
    for stmt in json.loads(body):
        names = [e["name"].get("some", "?") for e in stmt["schema"]["elements"]]
        out.extend(dict(zip(names, map(_plain, row))) for row in stmt["rows"])
    return out


def variant(v, names):
    return names[v[0]] if isinstance(v, list) and v and isinstance(v[0], int) else str(v)


def option(v):
    if isinstance(v, list) and len(v) == 2 and isinstance(v[0], int):
        return v[1] if v[0] == 0 else None
    if isinstance(v, dict):
        return v.get("some")
    return v


def read_token(path):
    try:
        tok = path.read_text().strip()
        return tok or None
    except FileNotFoundError:
        return None


def write_secret(path, text):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text)
    path.chmod(0o600)


def owner_token():
    m = re.search(r'^spacetimedb_token\s*=\s*"([^"]+)"', CLI_CONFIG.read_text(), re.M) if CLI_CONFIG.exists() else None
    if not m:
        raise Fail(f"no token in {CLI_CONFIG}; run setup_local.sh")
    return m.group(1)


# ---------- subcommands ----------


def ensure_owner():
    tok = None
    if CLI_CONFIG.exists():
        m = re.search(r'^spacetimedb_token\s*=\s*"([^"]+)"', CLI_CONFIG.read_text(), re.M)
        tok = m.group(1) if m else None
    if tok and token_valid(tok):
        print(f"owner identity {token_identity(tok)} (existing {CLI_CONFIG})")
        return
    ident, tok = new_identity()
    write_secret(
        CLI_CONFIG,
        f'default_server = "local"\nspacetimedb_token = "{tok}"\n\n'
        f'[[server_configs]]\nnickname = "local"\nhost = "127.0.0.1:{PORT}"\nprotocol = "http"\n',
    )
    print(f"owner identity {ident} (new {CLI_CONFIG})")


def db_owner():
    status, body = http("GET", f"/v1/database/{DB}")
    if status == 404:
        print("")
        return
    if status != 200:
        raise Fail(f"GET /v1/database/{DB} -> {status} {body}")
    print(json.loads(body)["owner_identity"]["__identity__"].removeprefix("0x"))


def ensure_admin():
    tok = owner_token()
    me = token_identity(tok)
    if sql(tok, f"SELECT * FROM admin WHERE human_identity = 0x{me}"):
        print(f"admin: {me} (already)")
        return
    # Owner-only SQL DML. init seeded SEED_ADMIN_HEX (the upstream admin), and
    # bootstrap_admin refuses while any admin exists.
    sql(tok, "DELETE FROM admin")
    must_call(tok, "bootstrap_admin")
    print(f"admin: {me} (bootstrapped)")


def ensure_sims():
    tok = owner_token()
    have = {r["name"] for r in sql(tok, "SELECT * FROM bot WHERE is_simulated = true")}
    for name, strategy in SIMS:
        if name not in have:
            must_call(tok, "spawn_simulated_bot", name, {strategy: []})
            print(f"sim: spawned {name} ({strategy})")
    sims = sql(tok, "SELECT * FROM bot WHERE is_simulated = true")
    print(f"sims: {len(sims)} ({', '.join(sorted(r['name'] for r in sims))})")


def credential_bot(owner, ident):
    rows = sql(owner, f"SELECT * FROM bot_credential WHERE identity = 0x{ident}")
    return rows[0]["bot_id"] if rows else None


def ensure_bot(owner, name):
    tok_path = TOKENS / f"{name}.token"
    human_path = TOKENS / f"{name}.human.token"
    bot = next(iter(sql(owner, f"SELECT * FROM bot WHERE name = '{name}'")), None)

    bot_tok = read_token(tok_path)
    if bot_tok and not token_valid(bot_tok):
        bot_tok = None
    if bot and bot_tok and credential_bot(owner, token_identity(bot_tok)) == bot["id"]:
        return bot["id"], token_identity(bot_tok), "ok"

    human = read_token(human_path)
    if human and not token_valid(human):
        human = None
    if bot is None:
        if human is None:
            _, human = new_identity()
            write_secret(human_path, human + "\n")
        must_call(human, "create_team", f"team-{name}", name)
        bot = next(iter(sql(owner, f"SELECT * FROM bot WHERE name = '{name}'")))
    elif human is None:
        raise Fail(
            f"bot '{name}' exists but its team owner's token ({human_path}) is missing; "
            "pick another name or run setup_local.sh --reset"
        )
    # `user` is a reserved word in SpacetimeDB SQL, so filter here.
    human_id = token_identity(human).lower()
    member = [r for r in sql(owner, "SELECT * FROM team_member") if r["user"].lower().removeprefix("0x") == human_id]
    if not member or member[0]["team_id"] != bot["team_id"]:
        raise Fail(f"{human_path} is not on bot '{name}''s team")

    must_call(human, "mint_credential_nonce")
    nonces = [n for n in sql(human, "SELECT * FROM my_nonces") if n["bot_id"] == bot["id"]]
    code = max(nonces, key=lambda n: n["id"])["code"]
    # Reuse the token we already have if its identity is free, so token
    # files survive a database reset; otherwise use a fresh identity.
    if not bot_tok or credential_bot(owner, token_identity(bot_tok)) is not None:
        _, bot_tok = new_identity()
    must_call(bot_tok, "claim_credential", code)
    ident = token_identity(bot_tok)
    if credential_bot(owner, ident) != bot["id"]:
        raise Fail(f"claim for '{name}' did not land")
    write_secret(tok_path, bot_tok + "\n")
    return bot["id"], ident, "claimed"


def ensure_bots(names):
    owner = owner_token()
    for name in names:
        bot_id, ident, how = ensure_bot(owner, name)
        print(f"bot {name:<8} id={bot_id:<4} credential={ident[:16]}.. {how:<8} {TOKENS / (name + '.token')}")


def report(match_ids):
    owner = owner_token()
    bots = {r["id"]: r for r in sql(owner, "SELECT * FROM bot")}
    worst = 0
    for mid in match_ids:
        m = next(iter(sql(owner, f"SELECT * FROM match_state WHERE id = {mid}")), None)
        if m is None:
            print(f"match {mid}: not found")
            worst = 1
            continue
        status = variant(m["status"], MATCH_STATUS)
        parts = sql(owner, f"SELECT * FROM match_participant WHERE match_id = {mid}")
        words = sql(owner, f"SELECT * FROM word_play WHERE match_id = {mid}")
        results = sql(owner, f"SELECT * FROM auction_result WHERE match_id = {mid}")
        unsold = 0
        wins = {}
        for r in results:
            w = option(r["winner_bot_id"])
            if w is None:
                unsold += 1
            else:
                wins.setdefault(w, []).append(r["paid"])
        print(
            f"match {mid}: status={status} rounds={m['current_round']} auctions={len(results)} "
            f"unsold={unsold} words={len(words)}"
        )
        print(f"  {'#':<3}{'bot':<16}{'id':>4}  {'score':>6} {'balance':>8} {'tiles':>6} {'paid':>5} {'words':>6}  kind")
        for i, p in enumerate(sorted(parts, key=lambda p: (-p["score"], p["bot_id"])), 1):
            b = bots.get(p["bot_id"], {})
            mine = [w for w in words if w["bot_id"] == p["bot_id"]]
            won = wins.get(p["bot_id"], [])
            print(
                f"  {i:<3}{b.get('name', '?'):<16}{p['bot_id']:>4}  {p['score']:>6} {p['balance']:>8} "
                f"{len(won):>6} {sum(won):>5} {len(mine):>6}  {'sim' if b.get('is_simulated') else 'real'}"
            )
        if status != "Ended":
            worst = 1
    return worst


STANDING = re.compile(r"(\d+)\.(\S+?)\((\d+)(?:,sim)?\) score=(-?\d+) balance=(-?\d+)")


def check_logs(log_dir):
    owner = owner_token()
    checked = bad = 0
    for log in sorted(pathlib.Path(log_dir).glob("*.log")):
        for line in log.read_text().splitlines():
            m = re.search(r"MATCH_END match=(\d+) .*standings: (.*)$", line)
            if not m:
                continue
            mid = int(m.group(1))
            truth = {p["bot_id"]: p for p in sql(owner, f"SELECT * FROM match_participant WHERE match_id = {mid}")}
            seen = STANDING.findall(m.group(2))
            if len(seen) != len(truth):
                print(f"FAIL {log.name} match {mid}: {len(seen)} standings, database has {len(truth)}")
                bad += 1
            for _, name, bot_id, score, balance in seen:
                p = truth.get(int(bot_id))
                checked += 1
                if p is None or (p["score"], p["balance"]) != (int(score), int(balance)):
                    want = (p["score"], p["balance"]) if p else None
                    print(f"FAIL {log.name} match {mid} {name}: logged score/balance {score}/{balance}, database {want}")
                    bad += 1
    verdict = "PASS" if checked and not bad else "FAIL"
    print(f"{verdict}: {checked} logged final standings checked against the database, {bad} mismatches")
    return 0 if verdict == "PASS" else 1


def status():
    owner = owner_token()
    bots = sql(owner, "SELECT * FROM bot")
    creds = sql(owner, "SELECT * FROM bot_credential")
    print(f"bots: {len(bots)} ({sum(1 for b in bots if b['is_simulated'])} sims), credentials: {len(creds)}")
    for b in bots:
        if not b["is_simulated"]:
            n = sum(1 for c in creds if c["bot_id"] == b["id"])
            print(f"  {b['name']:<10} id={b['id']:<4} team={b['team_id']:<4} credentials={n}")
    lobbies = sql(owner, "SELECT * FROM open_lobbies")
    for l in lobbies:
        members = sql(owner, f"SELECT * FROM lobby_member WHERE lobby_id = {l['id']}")
        print(f"open lobby {l['id']}: {len(members)}/{l['max_size']} members")
    running = [m for m in sql(owner, "SELECT * FROM match_state") if variant(m["status"], MATCH_STATUS) == "Running"]
    print(f"running matches: {[m['id'] for m in running]}")


def main(argv):
    if not argv:
        print(__doc__)
        return 2
    cmd, args = argv[0], argv[1:]
    try:
        if cmd == "ensure-owner":
            ensure_owner()
        elif cmd == "db-owner":
            db_owner()
        elif cmd == "ensure-admin":
            ensure_admin()
        elif cmd == "ensure-sims":
            ensure_sims()
        elif cmd == "ensure-bots":
            ensure_bots(args)
        elif cmd == "report":
            return report(args)
        elif cmd == "check-logs":
            return check_logs(args[0])
        elif cmd == "status":
            status()
        else:
            print(__doc__)
            return 2
    except Fail as e:
        print(f"error: {e}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
