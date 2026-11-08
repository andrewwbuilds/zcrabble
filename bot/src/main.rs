//! zcrabble-bot: a ScrabbleBot client (SpacetimeDB Rust SDK 2.14.1).
//!
//! Environment:
//!   STDB_HOST         default https://maincloud.spacetimedb.com (local e2e: http://127.0.0.1:3300)
//!   STDB_DB           default scrabblebot
//!   BOT_TOKEN         token (JWT) of a bot credential, or
//!   BOT_TOKEN_FILE    a file holding it
//!   BOT_NONCE         optional: claim a credential with this nonce if the token has none (or there
//!                     is no token); the new token is written to BOT_TOKEN_FILE
//!   BOT_NAME          optional log label (default: the bot's name)
//!   BOT_SUBSCRIBE     scoped (default) | all. `all` is subscribe_to_all_tables: fine locally, but
//!                     about 6.5M rows on maincloud (auction, auction_result and the top-bid
//!                     archive are ~1.8M rows each).
//!   BOT_MAX_MATCHES   exit 0 once this many matches have ended (default: play forever)
//!   BOT_JOIN_LOBBY    1 (default) | 0
//!   BOT_MIN_WORD_LEN  shortest word the stub strategy plays (default 2)
//!   TOURNAMENT_ID     optional: register for this tournament once connected
//!   BOT_VERBOSE       1: also log other bots' balance changes and lobby openings
//!
//! Every log line is `<UTC timestamp> [<bot>] <EVENT> key=value ...`.

mod dict;
mod engine;
mod module_bindings;
mod strategy;
mod strategy_stub;

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use module_bindings::*;
use spacetimedb_sdk::SubscriptionHandle as _;
use spacetimedb_sdk::{DbContext, Event, Identity, Table, TableWithPrimaryKey};

use dict::{Dictionary, Rack};
use strategy_stub::{
    AuctionKind, AuctionOutcome, BidContext, MatchInfo, Participant, Strategy, WordContext,
    WordPlayed,
};

// ---------- logging ----------

static LABEL: Mutex<String> = Mutex::new(String::new());

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn now_micros() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or(0)
}

/// `micros` since the Unix epoch as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
fn fmt_ts(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let ms = micros.rem_euclid(1_000_000) / 1000;
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        sod / 3600,
        sod % 3600 / 60,
        sod % 60
    )
}

macro_rules! log {
    ($($arg:tt)*) => {{
        let line = format!("{} [{}] {}\n", fmt_ts(now_micros()), lock(&LABEL), format_args!($($arg)*));
        let _ = std::io::stdout().lock().write_all(line.as_bytes());
    }};
}

/// Runs a callback body, logging a panic instead of letting it kill the
/// SDK's message thread.
fn guard(what: &str, f: impl FnOnce()) {
    if let Err(e) = catch_unwind(AssertUnwindSafe(f)) {
        let msg = e
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| e.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "?".into());
        log!("PANIC in {what}: {msg}");
    }
}

// ---------- config ----------

#[derive(Clone)]
struct Config {
    host: String,
    db: String,
    token: Option<String>,
    token_file: Option<PathBuf>,
    nonce: Option<String>,
    name: Option<String>,
    subscribe_all: bool,
    max_matches: Option<u32>,
    join_lobby: bool,
    min_word_len: usize,
    tournament_id: Option<u64>,
    verbose: bool,
}

fn env_nonempty(key: &str) -> Option<String> {
    std::env::var(key)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn env_u64(key: &str) -> Result<Option<u64>, String> {
    env_nonempty(key)
        .map(|v| v.parse::<u64>().map_err(|e| format!("{key}={v}: {e}")))
        .transpose()
}

fn env_flag(key: &str, default: bool) -> bool {
    match env_nonempty(key).as_deref() {
        None => default,
        Some("0" | "false" | "no" | "off") => false,
        Some(_) => true,
    }
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let host = env_nonempty("STDB_HOST")
            .unwrap_or_else(|| "https://maincloud.spacetimedb.com".into())
            .trim_end_matches('/')
            .to_string();
        if !["http://", "https://", "ws://", "wss://"]
            .iter()
            .any(|p| host.starts_with(p))
        {
            return Err(format!("STDB_HOST must be an http(s):// URL, got {host}"));
        }
        let token_file = env_nonempty("BOT_TOKEN_FILE").map(PathBuf::from);
        let token = env_nonempty("BOT_TOKEN").or_else(|| {
            let path = token_file.as_ref()?;
            let s = std::fs::read_to_string(path).ok()?;
            Some(s.trim().to_string()).filter(|s| !s.is_empty())
        });
        let nonce = env_nonempty("BOT_NONCE");
        if token.is_none() && nonce.is_none() {
            return Err(match &token_file {
                Some(p) => format!("BOT_TOKEN_FILE {} is missing or empty (and no BOT_NONCE)", p.display()),
                None => "set BOT_TOKEN or BOT_TOKEN_FILE (or BOT_NONCE to claim a credential)".into(),
            });
        }
        let subscribe_all = match env_nonempty("BOT_SUBSCRIBE").as_deref() {
            None | Some("scoped") => false,
            Some("all") => true,
            Some(other) => return Err(format!("BOT_SUBSCRIBE must be scoped or all, not {other}")),
        };
        Ok(Config {
            host,
            db: env_nonempty("STDB_DB").unwrap_or_else(|| "scrabblebot".into()),
            token,
            token_file,
            nonce,
            name: env_nonempty("BOT_NAME"),
            subscribe_all,
            max_matches: env_u64("BOT_MAX_MATCHES")?.map(|n| n as u32),
            join_lobby: env_flag("BOT_JOIN_LOBBY", true),
            min_word_len: env_u64("BOT_MIN_WORD_LEN")?.unwrap_or(2) as usize,
            tournament_id: env_u64("TOURNAMENT_ID")?,
            verbose: env_flag("BOT_VERBOSE", false),
        })
    }
}

// ---------- shared state ----------

enum Signal {
    /// The connection is gone; reconnect.
    Disconnected(String),
    /// Played BOT_MAX_MATCHES; exit 0.
    Done,
    /// Can't continue (e.g. the token is not a bot credential); exit 2.
    Fatal(String),
}

/// One bid we sent, for latency stats.
#[derive(Default, Clone, Copy)]
struct Lat {
    /// Auction opened (server clock) -> bid transaction (server clock).
    server_ms: f64,
    /// Auction opened (server clock) -> row callback ran here (local clock).
    recv_ms: f64,
    /// submit_bid sent -> its result processed here (local clock).
    rtt_ms: f64,
}

#[derive(Default)]
struct State {
    identity: Option<Identity>,
    /// The token in use (the one we connected with, or the one the server
    /// issued to an anonymous connection that then claimed a nonce).
    token: Option<String>,
    bot_id: Option<u64>,
    // Per connection; reset on reconnect.
    scope_requested: bool,
    ready: bool,
    connected_at: Option<Instant>,
    lobby_inflight: bool,
    last_lobby_try: Option<Instant>,
    words_inflight: HashMap<u64, usize>,
    claim_inflight: bool,
    registered_tournament: bool,
    /// Tournaments we've asked to register for (auto-registration).
    auto_registered: HashSet<u64>,
    /// Last match event received (auction, result, balance). Auctions open
    /// every second, so a long silence mid-match means a stalled connection.
    last_rx: Option<Instant>,
    /// Last reconnect made to repair the server's `connected` flag (kept
    /// across connections, to rate-limit those reconnects).
    last_flag_refresh: Option<Instant>,
    /// The match-scope subscription, and ones it replaced that are dropped
    /// once it's applied.
    scope: Option<SubscriptionHandle>,
    old_scopes: Vec<SubscriptionHandle>,
    // Across connections.
    bids: HashSet<u64>,
    rejected_words: HashSet<(u64, String)>,
    last_rack_logged: HashMap<u64, String>,
    announced: HashSet<u64>,
    ended: HashSet<u64>,
    matches_finished: u32,
    lat_by_match: HashMap<u64, Vec<Lat>>,
    lat_all: Vec<Lat>,
    done: bool,
}

struct Shared {
    cfg: Config,
    dict: &'static Dictionary,
    st: Mutex<State>,
    strat: Mutex<Box<dyn Strategy>>,
    signal: Mutex<Option<Sender<Signal>>>,
}

impl Shared {
    fn bot_id(&self) -> Option<u64> {
        lock(&self.st).bot_id
    }
    fn send(&self, s: Signal) {
        if let Some(tx) = lock(&self.signal).as_ref() {
            let _ = tx.send(s);
        }
    }
}

// ---------- cache helpers ----------

fn is_initial(ev: &Event<Reducer>) -> bool {
    matches!(ev, Event::SubscribeApplied)
}

fn letter_char(s: &str) -> char {
    s.chars().next().unwrap_or('?')
}

fn bot_name<C: RemoteDbContext>(ctx: &C, bot_id: u64) -> String {
    ctx.db()
        .bot()
        .id()
        .find(&bot_id)
        .map(|b| b.name)
        .unwrap_or_else(|| format!("#{bot_id}"))
}

fn participants<C: RemoteDbContext>(ctx: &C, match_id: u64) -> Vec<Participant> {
    let mut out: Vec<Participant> = ctx
        .db()
        .match_participant()
        .iter()
        .filter(|p| p.match_id == match_id)
        .map(|p| {
            let bot = ctx.db().bot().id().find(&p.bot_id);
            Participant {
                bot_id: p.bot_id,
                name: bot.as_ref().map_or_else(|| format!("#{}", p.bot_id), |b| b.name.clone()),
                is_sim: bot.as_ref().is_some_and(|b| b.is_simulated),
                balance: p.balance,
                score: p.score,
            }
        })
        .collect();
    out.sort_by_key(|p| p.bot_id);
    out
}

fn i_am_in<C: RemoteDbContext>(ctx: &C, match_id: u64, me: u64) -> bool {
    ctx.db()
        .match_participant()
        .iter()
        .any(|p| p.match_id == match_id && p.bot_id == me)
}

fn running_match<C: RemoteDbContext>(ctx: &C, match_id: u64) -> Option<Match> {
    ctx.db()
        .match_state()
        .id()
        .find(&match_id)
        .filter(|m| matches!(m.status, MatchStatus::Running))
}

fn my_running_matches<C: RemoteDbContext>(ctx: &C, me: u64) -> Vec<u64> {
    let mut ids: Vec<u64> = ctx
        .db()
        .match_participant()
        .iter()
        .filter(|p| p.bot_id == me && running_match(ctx, p.match_id).is_some())
        .map(|p| p.match_id)
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

fn rack_for<C: RemoteDbContext>(ctx: &C, match_id: u64) -> Rack {
    let mut rack = [0u32; 26];
    for h in ctx.db().my_rack().iter().filter(|h| h.match_id == match_id) {
        dict::rack_add(&mut rack, &h.letter, h.count);
    }
    rack
}

fn auction_kind(t: &AuctionType) -> AuctionKind {
    match t {
        AuctionType::FirstPrice => AuctionKind::FirstPrice,
        AuctionType::Vickrey => AuctionKind::Vickrey,
    }
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

fn lat_summary(lats: &[Lat]) -> String {
    let stat = |f: fn(&Lat) -> f64| {
        let mut v: Vec<f64> = lats.iter().map(f).collect();
        v.sort_by(f64::total_cmp);
        format!(
            "p50={:.1} p95={:.1} max={:.1}",
            pct(&v, 0.5),
            pct(&v, 0.95),
            v.last().copied().unwrap_or(f64::NAN)
        )
    };
    format!(
        "n={} open->bid_tx(server) {} | open->recv {} | bid rtt {}",
        lats.len(),
        stat(|l| l.server_ms),
        stat(|l| l.recv_ms),
        stat(|l| l.rtt_ms)
    )
}

// ---------- subscriptions ----------

const BASE_QUERIES: &[&str] = &[
    "SELECT * FROM bot_credential WHERE identity = :sender",
    "SELECT * FROM bot",
    "SELECT * FROM open_lobbies",
    "SELECT * FROM open_lobby_members",
    "SELECT * FROM my_rack",
    "SELECT * FROM matchmaker_config",
    "SELECT * FROM tournament",
];

/// Rows of the matches we play in from match `m0` on. Joining through
/// `match_participant` delivers a new match's first auction in the same update
/// as our seat in it.
///
/// The participants of our matches would be a self-join of
/// `match_participant`, but the 2.14.1 host sends a self-join's initial rows
/// and then only updates to the filtered alias's row (ours), so opponents'
/// balances and scores would go stale. Every match's participants from `m0` on
/// is a plain filter that stays current, and we re-scope after each match.
fn scope_queries(me: u64, m0: u64) -> Vec<String> {
    let via_seat = |t: &str, col: &str| {
        format!(
            "SELECT {t}.* FROM {t} JOIN match_participant ON {t}.{col} = match_participant.match_id \
             WHERE match_participant.bot_id = {me} AND match_participant.match_id >= {m0}"
        )
    };
    vec![
        via_seat("match_state", "id"),
        via_seat("auction", "match_id"),
        via_seat("auction_result", "match_id"),
        via_seat("word_play", "match_id"),
        format!("SELECT * FROM match_participant WHERE match_id >= {m0}"),
    ]
}

fn on_connect(conn: &DbConnection, sh: &Arc<Shared>, identity: Identity, token: &str) {
    {
        let mut st = lock(&sh.st);
        st.identity = Some(identity);
        st.token = Some(token.to_string());
    }
    log!(
        "CONNECTED identity={} host={} db={} subscribe={}",
        identity.to_hex(),
        sh.cfg.host,
        sh.cfg.db,
        if sh.cfg.subscribe_all { "all" } else { "scoped" }
    );
    let (sh_ok, sh_err) = (sh.clone(), sh.clone());
    let builder = conn
        .subscription_builder()
        .on_applied(move |ctx| guard("base on_applied", || on_base_applied(ctx, &sh_ok)))
        .on_error(move |_ctx, err| {
            log!("SUBSCRIBE_ERROR base: {err}");
            sh_err.send(Signal::Disconnected(format!("base subscription failed: {err}")));
        });
    if sh.cfg.subscribe_all {
        if !(sh.cfg.host.contains("127.0.0.1") || sh.cfg.host.contains("localhost")) {
            log!("WARN BOT_SUBSCRIBE=all on a remote host downloads every table (millions of rows on maincloud)");
        }
        builder.subscribe_to_all_tables();
    } else {
        builder.subscribe(BASE_QUERIES.to_vec());
    }
}

fn on_base_applied<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>) {
    let Some(me) = ctx.try_identity() else { return };
    log!("SUBSCRIBED base");
    if let Some(cred) = ctx.db().bot_credential().identity().find(&me) {
        resolve_bot(ctx, sh, cred.bot_id);
        return;
    }
    let Some(nonce) = sh.cfg.nonce.clone() else {
        sh.send(Signal::Fatal(format!(
            "identity {} is not a credential for any bot; get a token from the team page or set BOT_NONCE",
            me.to_hex()
        )));
        return;
    };
    if std::mem::replace(&mut lock(&sh.st).claim_inflight, true) {
        return;
    }
    log!("CLAIM nonce={nonce}");
    let sh2 = sh.clone();
    let sent = ctx.reducers().claim_credential_then(nonce, move |_rctx, res| {
        guard("claim result", || {
            lock(&sh2.st).claim_inflight = false;
            match res {
                // The credential row arrives through the bot_credential
                // subscription; its on_insert resolves the bot.
                Ok(Ok(())) => {
                    log!("CLAIM_OK");
                    let token = lock(&sh2.st).token.clone();
                    if let (Some(path), Some(token)) = (&sh2.cfg.token_file, token) {
                        match std::fs::write(path, format!("{token}\n")) {
                            Ok(()) => log!("TOKEN_SAVED path={}", path.display()),
                            Err(e) => log!("TOKEN_SAVE_FAILED path={} err={e}", path.display()),
                        }
                    }
                }
                Ok(Err(msg)) => sh2.send(Signal::Fatal(format!("claim_credential rejected: {msg}"))),
                Err(e) => sh2.send(Signal::Fatal(format!("claim_credential failed: {e}"))),
            }
        })
    });
    if let Err(e) = sent {
        log!("CLAIM_SEND_FAILED err={e}");
        lock(&sh.st).claim_inflight = false;
    }
}

fn resolve_bot<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, bot_id: u64) {
    {
        let mut st = lock(&sh.st);
        if st.scope_requested {
            return;
        }
        st.bot_id = Some(bot_id);
        st.scope_requested = true;
    }
    let name = bot_name(ctx, bot_id);
    if sh.cfg.name.is_none() {
        *lock(&LABEL) = name.clone();
    }
    log!("BOT id={bot_id} name={name} strategy={}", lock(&sh.strat).name());
    if sh.cfg.subscribe_all {
        on_scope_ready(ctx, sh, 0);
        return;
    }
    // Our seat history, to start the match scope at our latest match: any
    // match we're in now is that one, and new matches have higher ids.
    let (sh_ok, sh_err) = (sh.clone(), sh.clone());
    ctx.subscription_builder()
        .on_applied(move |ctx| {
            guard("seats on_applied", || {
                let m0 = ctx
                    .db()
                    .match_participant()
                    .iter()
                    .filter(|p| p.bot_id == bot_id)
                    .map(|p| p.match_id)
                    .max()
                    .unwrap_or(0);
                subscribe_scope(ctx, &sh_ok, bot_id, m0);
            })
        })
        .on_error(move |_ctx, err| {
            log!("SUBSCRIBE_ERROR seats: {err}");
            sh_err.send(Signal::Disconnected(format!("seat subscription failed: {err}")));
        })
        .subscribe(vec![
            format!("SELECT * FROM match_participant WHERE bot_id = {bot_id}"),
            format!("SELECT * FROM tournament_entry WHERE bot_id = {bot_id}"),
        ]);
}

/// Subscribes to our matches from `m0` on. A later call replaces the earlier
/// scope once the new one is applied, so no update falls in between.
fn subscribe_scope<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, me: u64, m0: u64) {
    let (sh_ok, sh_err) = (sh.clone(), sh.clone());
    let handle = ctx
        .subscription_builder()
        .on_applied(move |ctx| {
            guard("scope on_applied", || {
                let (old, first) = {
                    let mut st = lock(&sh_ok.st);
                    (std::mem::take(&mut st.old_scopes), !st.ready)
                };
                for h in old {
                    let _ = h.unsubscribe();
                }
                if first {
                    on_scope_ready(ctx, &sh_ok, m0);
                } else {
                    log!(
                        "RESCOPED match_scope>={m0} cached: participants={} matches={} auctions={}",
                        ctx.db().match_participant().count(),
                        ctx.db().match_state().count(),
                        ctx.db().auction().count()
                    );
                }
            })
        })
        .on_error(move |_ctx, err| {
            log!("SUBSCRIBE_ERROR scope: {err}");
            sh_err.send(Signal::Disconnected(format!("scope subscription failed: {err}")));
        })
        .subscribe(scope_queries(me, m0));
    let mut st = lock(&sh.st);
    if let Some(prev) = st.scope.replace(handle) {
        st.old_scopes.push(prev);
    }
}

fn on_scope_ready<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, m0: u64) {
    let Some(me) = sh.bot_id() else { return };
    {
        let mut st = lock(&sh.st);
        st.ready = true;
        st.last_rx = Some(Instant::now());
    }
    let running = my_running_matches(ctx, me);
    log!(
        "READY match_scope>={m0} cached: participants={} matches={} auctions={} rack_rows={} running={running:?}",
        ctx.db().match_participant().count(),
        ctx.db().match_state().count(),
        ctx.db().auction().count(),
        ctx.db().my_rack().count()
    );
    for mid in &running {
        announce_match(ctx, sh, *mid);
    }
    // Resuming mid-match: bid on the open auction and play what we can.
    for a in ctx.db().auction().iter() {
        if matches!(a.status, AuctionStatus::Open) && running.contains(&a.match_id) {
            try_bid(ctx, sh, &a, now_micros());
        }
    }
    for mid in &running {
        try_play_word(ctx, sh, *mid);
    }
    if let Some(tid) = sh.cfg.tournament_id {
        if !std::mem::replace(&mut lock(&sh.st).registered_tournament, true) {
            log!("TOURNAMENT_REGISTER id={tid}");
            let _ = ctx.reducers().register_for_tournament_then(tid, move |_r, res| {
                match res {
                    Ok(Ok(())) => log!("TOURNAMENT_REGISTER_OK id={tid}"),
                    Ok(Err(m)) => log!("TOURNAMENT_REGISTER_REJECTED id={tid} reason={m}"),
                    Err(e) => log!("TOURNAMENT_REGISTER_ERROR id={tid} err={e}"),
                }
            });
        }
    }
    maybe_register_tournaments(ctx, sh);
    maybe_join_lobby(ctx, sh, "startup");
}

/// Registers for every tournament that is open for registration (unless
/// AUTO_TOURNAMENT=0). The module only accepts this from the bot's own
/// credential, so it has to come from this process.
fn maybe_register_tournaments<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>) {
    if !env_flag("AUTO_TOURNAMENT", true) {
        return;
    }
    {
        let st = lock(&sh.st);
        if !st.ready || st.bot_id.is_none() {
            return;
        }
    }
    let open: Vec<u64> = ctx
        .db()
        .tournament()
        .iter()
        .filter(|t| matches!(t.status, TournamentStatus::Registration))
        .map(|t| t.id)
        .collect();
    for tid in open {
        if !lock(&sh.st).auto_registered.insert(tid) {
            continue;
        }
        log!("TOURNAMENT_AUTO_REGISTER id={tid}");
        let sh2 = sh.clone();
        let _ = ctx.reducers().register_for_tournament_then(tid, move |_r, res| match res {
            Ok(Ok(())) => log!("TOURNAMENT_REGISTER_OK id={tid}"),
            Ok(Err(m)) => {
                log!("TOURNAMENT_REGISTER_REJECTED id={tid} reason={m}");
                lock(&sh2.st).auto_registered.remove(&tid);
            }
            Err(e) => {
                log!("TOURNAMENT_REGISTER_ERROR id={tid} err={e}");
                lock(&sh2.st).auto_registered.remove(&tid);
            }
        });
    }
}

// ---------- lobby ----------

fn maybe_join_lobby<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, reason: &str) {
    if !sh.cfg.join_lobby {
        return;
    }
    let me = {
        let st = lock(&sh.st);
        if st.done || !st.ready || st.lobby_inflight {
            return;
        }
        if st.last_lobby_try.is_some_and(|t| t.elapsed() < Duration::from_secs(3)) {
            return;
        }
        match st.bot_id {
            Some(me) => me,
            None => return,
        }
    };
    if !my_running_matches(ctx, me).is_empty() {
        return;
    }
    if ctx.db().open_lobby_members().iter().any(|m| m.bot_id == me) {
        return;
    }
    {
        let mut st = lock(&sh.st);
        st.lobby_inflight = true;
        st.last_lobby_try = Some(Instant::now());
    }
    log!("LOBBY_JOIN reason={reason}");
    let sh2 = sh.clone();
    let sent = ctx.reducers().join_lobby_then(move |_rctx, res| {
        let mut st = lock(&sh2.st);
        st.lobby_inflight = false;
        match res {
            Ok(Ok(())) => log!("LOBBY_JOIN_OK"),
            Ok(Err(m)) => {
                // E.g. held for a tournament: ask again in ~15 s, not every 3 s.
                st.last_lobby_try = Some(Instant::now() + Duration::from_secs(12));
                log!("LOBBY_JOIN_REJECTED reason={m}");
            }
            Err(e) => log!("LOBBY_JOIN_ERROR err={e}"),
        }
    });
    if let Err(e) = sent {
        log!("LOBBY_JOIN_SEND_FAILED err={e}");
        lock(&sh.st).lobby_inflight = false;
    }
}

// ---------- match lifecycle ----------

fn announce_match<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, match_id: u64) {
    let Some(me) = sh.bot_id() else { return };
    let Some(m) = running_match(ctx, match_id) else { return };
    if !lock(&sh.st).announced.insert(match_id) {
        return;
    }
    let parts = participants(ctx, match_id);
    let roster: Vec<String> = parts
        .iter()
        .map(|p| format!("{}({}{})", p.name, p.bot_id, if p.is_sim { ",sim" } else { "" }))
        .collect();
    log!(
        "MATCH_START match={match_id} type={:?} round={} bag={} rack_size={} auction_ms={} players=[{}] my_rack={}",
        m.auction_type,
        m.current_round,
        m.bag_total,
        m.starting_rack_size,
        m.auction_duration_ms,
        roster.join(" "),
        dict::rack_string(&rack_for(ctx, match_id))
    );
    lock(&sh.strat).on_match_start(&MatchInfo {
        match_id,
        my_bot_id: me,
        auction_kind: auction_kind(&m.auction_type),
        auction_duration_ms: m.auction_duration_ms,
        starting_rack_size: m.starting_rack_size,
        participants: parts,
    });
}

fn on_match_ended<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, m: &Match) {
    let Some(me) = sh.bot_id() else { return };
    let (finished, lats) = {
        let mut st = lock(&sh.st);
        if !st.ended.insert(m.id) {
            return;
        }
        st.matches_finished += 1;
        (st.matches_finished, st.lat_by_match.remove(&m.id).unwrap_or_default())
    };
    let mut standings = participants(ctx, m.id);
    standings.sort_by(|a, b| b.score.cmp(&a.score).then(a.bot_id.cmp(&b.bot_id)));
    let table: Vec<String> = standings
        .iter()
        .enumerate()
        .map(|(i, p)| {
            format!(
                "{}.{}({}{}) score={} balance={}",
                i + 1,
                p.name,
                p.bot_id,
                if p.is_sim { ",sim" } else { "" },
                p.score,
                p.balance
            )
        })
        .collect();
    let place = standings.iter().position(|p| p.bot_id == me).map_or(0, |i| i + 1);
    log!("MATCH_END match={} rounds={} place={place}/{} standings: {}", m.id, m.current_round, standings.len(), table.join(" | "));
    let won: Vec<AuctionResult> = ctx
        .db()
        .auction_result()
        .iter()
        .filter(|r| r.match_id == m.id && r.winner_bot_id == Some(me))
        .collect();
    let words: Vec<WordPlay> = ctx
        .db()
        .word_play()
        .iter()
        .filter(|w| w.match_id == m.id && w.bot_id == me)
        .collect();
    let leftover = rack_for(ctx, m.id);
    log!(
        "MATCH_SUMMARY match={} tiles_won={} paid={} words={} [{}] word_reward={} leftover={} ({} tiles, face {})",
        m.id,
        won.len(),
        won.iter().map(|r| r.paid).sum::<i64>(),
        words.len(),
        words.iter().map(|w| w.word.as_str()).collect::<Vec<_>>().join(","),
        words.iter().map(|w| w.total_reward).sum::<i64>(),
        dict::rack_string(&leftover),
        dict::rack_size(&leftover),
        dict::rack_string(&leftover).chars().map(dict::letter_value).sum::<i64>()
    );
    if !lats.is_empty() {
        log!("LATENCY match={} {}", m.id, lat_summary(&lats));
    }
    lock(&sh.strat).on_match_end(m.id, &standings);
    if sh.cfg.max_matches.is_some_and(|max| finished >= max) {
        lock(&sh.st).done = true;
        log!("DONE matches_finished={finished}");
        sh.send(Signal::Done);
        return;
    }
    // Drop this match's rows (and other matches' participants) from the
    // cache: new matches get higher ids.
    if !sh.cfg.subscribe_all {
        let m0 = my_running_matches(ctx, me).into_iter().fold(m.id + 1, u64::min);
        subscribe_scope(ctx, sh, me, m0);
    }
    maybe_join_lobby(ctx, sh, "match ended");
}

// ---------- bidding ----------

fn try_bid<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, a: &Auction, recv_micros: i64) {
    let Some(me) = sh.bot_id() else { return };
    if !matches!(a.status, AuctionStatus::Open) {
        return;
    }
    let Some(m) = running_match(ctx, a.match_id) else { return };
    let parts = participants(ctx, a.match_id);
    let Some(mine) = parts.iter().find(|p| p.bot_id == me).cloned() else { return };
    if !lock(&sh.st).bids.insert(a.id) {
        return;
    }
    announce_match(ctx, sh, a.match_id);
    let letter = letter_char(&a.letter);
    let opens = a.opens_at.to_micros_since_unix_epoch();
    let closes = a.closes_at.to_micros_since_unix_epoch();
    let rack = rack_for(ctx, a.match_id);
    let bctx = BidContext {
        match_id: a.match_id,
        auction_id: a.id,
        letter,
        round: m.current_round,
        bag_remaining: m.bag_total,
        auction_kind: auction_kind(&m.auction_type),
        my_balance: mine.balance,
        my_score: mine.score,
        rack,
        participants: &parts,
        opens_at_micros: opens,
        closes_at_micros: closes,
        now_micros: now_micros(),
    };
    let t_dec = Instant::now();
    let (amount, note) = {
        let mut strat = lock(&sh.strat);
        let amount = strat.decide_bid(&bctx).clamp(0, mine.balance.max(0));
        (amount, strat.note())
    };
    let dec_ms = t_dec.elapsed().as_secs_f64() * 1000.0;
    if amount <= 0 {
        log!(
            "BID_SKIP auction={} match={} letter={letter} balance={} bag={} {note} dec_ms={dec_ms:.1}",
            a.id,
            a.match_id,
            mine.balance,
            m.bag_total
        );
        return;
    }
    let sent = now_micros();
    log!(
        "BID auction={} match={} round={} letter={letter} amount={amount} balance={} rack={} bag={} recv_ms={:.1} dec_ms={dec_ms:.1} {note}",
        a.id,
        a.match_id,
        m.current_round,
        mine.balance,
        dict::rack_string(&rack),
        m.bag_total,
        (recv_micros - opens) as f64 / 1000.0
    );
    let (sh2, auction_id, match_id) = (sh.clone(), a.id, a.match_id);
    let res = ctx.reducers().submit_bid_then(a.id, amount, move |rctx, res| {
        guard("bid result", || {
            let ack = now_micros();
            let server = rctx.event.timestamp.to_micros_since_unix_epoch();
            let lat = Lat {
                server_ms: (server - opens) as f64 / 1000.0,
                recv_ms: (recv_micros - opens) as f64 / 1000.0,
                rtt_ms: (ack - sent) as f64 / 1000.0,
            };
            match res {
                Ok(Ok(())) => {
                    log!(
                        "BID_OK auction={auction_id} amount={amount} open->bid_tx={:.1}ms recv={:.1}ms rtt={:.1}ms margin={:.1}ms",
                        lat.server_ms,
                        lat.recv_ms,
                        lat.rtt_ms,
                        (closes - server) as f64 / 1000.0
                    );
                    let mut st = lock(&sh2.st);
                    st.lat_by_match.entry(match_id).or_default().push(lat);
                    st.lat_all.push(lat);
                }
                Ok(Err(msg)) => log!(
                    "BID_REJECTED auction={auction_id} amount={amount} reason={msg} open->bid_tx={:.1}ms",
                    lat.server_ms
                ),
                Err(e) => log!("BID_ERROR auction={auction_id} amount={amount} err={e}"),
            }
        })
    });
    if let Err(e) = res {
        log!("BID_SEND_FAILED auction={} err={e}", a.id);
    }
}

// ---------- word play ----------

fn try_play_word<C: RemoteDbContext>(ctx: &C, sh: &Arc<Shared>, match_id: u64) {
    let Some(me) = sh.bot_id() else { return };
    let Some(m) = running_match(ctx, match_id) else { return };
    let rack = rack_for(ctx, match_id);
    let rack_s = dict::rack_string(&rack);
    {
        let mut st = lock(&sh.st);
        if st.last_rack_logged.get(&match_id) != Some(&rack_s) {
            st.last_rack_logged.insert(match_id, rack_s.clone());
            drop(st);
            log!("RACK match={match_id} rack={rack_s} ({} tiles) bag={}", dict::rack_size(&rack), m.bag_total);
        }
    }
    if lock(&sh.st).words_inflight.contains_key(&match_id) {
        return;
    }
    let parts = participants(ctx, match_id);
    let Some(mine) = parts.iter().find(|p| p.bot_id == me).cloned() else { return };
    let wctx = WordContext {
        match_id,
        rack,
        bag_remaining: m.bag_total,
        my_balance: mine.balance,
        my_score: mine.score,
        participants: &parts,
    };
    let t_dec = Instant::now();
    let chosen = lock(&sh.strat).choose_words(&wctx);
    let dec_ms = t_dec.elapsed().as_secs_f64() * 1000.0;
    // Keep only words that are valid and jointly spellable from the rack.
    let mut left = rack;
    let mut words: Vec<String> = Vec::new();
    for w in chosen {
        let w = w.trim().to_ascii_uppercase();
        if !sh.dict.contains(&w) || !dict::can_spell(&w, &left) {
            log!("PLAY_SKIP match={match_id} word={w} reason=not-in-dictionary-or-not-on-rack rack={rack_s}");
            continue;
        }
        if lock(&sh.st).rejected_words.contains(&(match_id, w.clone())) {
            continue;
        }
        for c in w.chars() {
            if let Some(i) = dict::letter_index(c) {
                left[i] -= 1;
            }
        }
        words.push(w);
    }
    if words.is_empty() {
        return;
    }
    {
        let mut st = lock(&sh.st);
        if st.words_inflight.contains_key(&match_id) {
            return;
        }
        st.words_inflight.insert(match_id, words.len());
    }
    for word in words {
        let expected = dict::word_reward(&word);
        log!(
            "PLAY match={match_id} word={word} expected_reward={expected} rack={rack_s} bag={} dec_ms={dec_ms:.1}",
            m.bag_total
        );
        let (sh2, w2) = (sh.clone(), word.clone());
        let res = ctx.reducers().submit_word_then(match_id, word.clone(), move |rctx, res| {
            guard("word result", || {
                let last = {
                    let mut st = lock(&sh2.st);
                    let n = st.words_inflight.get(&match_id).copied().unwrap_or(1);
                    if n <= 1 {
                        st.words_inflight.remove(&match_id);
                        true
                    } else {
                        st.words_inflight.insert(match_id, n - 1);
                        false
                    }
                };
                match res {
                    Ok(Ok(())) => log!("PLAY_OK match={match_id} word={w2} reward={expected}"),
                    Ok(Err(msg)) => {
                        log!("PLAY_REJECTED match={match_id} word={w2} reason={msg}");
                        lock(&sh2.st).rejected_words.insert((match_id, w2));
                    }
                    Err(e) => log!("PLAY_ERROR match={match_id} word={w2} err={e}"),
                }
                if last {
                    // More words may be playable from what's left.
                    try_play_word(rctx, &sh2, match_id);
                }
            })
        });
        if let Err(e) = res {
            log!("PLAY_SEND_FAILED match={match_id} word={word} err={e}");
            let mut st = lock(&sh.st);
            let n = st.words_inflight.get(&match_id).copied().unwrap_or(1);
            if n <= 1 {
                st.words_inflight.remove(&match_id);
            } else {
                st.words_inflight.insert(match_id, n - 1);
            }
        }
    }
}

// ---------- row callbacks ----------

fn register_callbacks(conn: &DbConnection, sh: &Arc<Shared>) {
    let s = sh.clone();
    conn.db.bot_credential().on_insert(move |ctx, row| {
        guard("bot_credential insert", || {
            if ctx.try_identity() == Some(row.identity) {
                resolve_bot(ctx, &s, row.bot_id);
            }
        })
    });

    conn.db.bot_credential().on_update(move |ctx, old, new| {
        guard("bot_credential update", || {
            if ctx.try_identity() == Some(new.identity) && old.connected != new.connected {
                log!("CRED_FLAG connected={}", new.connected);
            }
        })
    });

    let s = sh.clone();
    conn.db.match_participant().on_insert(move |ctx, p| {
        guard("match_participant insert", || {
            if s.bot_id() == Some(p.bot_id) && !is_initial(&ctx.event) {
                log!("SEATED match={} balance={} score={}", p.match_id, p.balance, p.score);
                announce_match(ctx, &s, p.match_id);
            }
        })
    });
    let s = sh.clone();
    conn.db.match_participant().on_update(move |ctx, old, new| {
        guard("match_participant update", || {
            if s.bot_id() == Some(new.bot_id) {
                log!(
                    "BALANCE match={} balance {}->{} score {}->{}",
                    new.match_id,
                    old.balance,
                    new.balance,
                    old.score,
                    new.score
                );
            } else if s.cfg.verbose {
                log!(
                    "OPP_BALANCE match={} bot={} balance {}->{} score {}->{}",
                    new.match_id,
                    bot_name(ctx, new.bot_id),
                    old.balance,
                    new.balance,
                    old.score,
                    new.score
                );
            }
        })
    });

    let s = sh.clone();
    conn.db.match_state().on_insert(move |ctx, m| {
        guard("match_state insert", || {
            if matches!(m.status, MatchStatus::Running) && s.bot_id().is_some_and(|me| i_am_in(ctx, m.id, me)) {
                announce_match(ctx, &s, m.id);
            }
        })
    });
    let s = sh.clone();
    conn.db.match_state().on_update(move |ctx, old, new| {
        guard("match_state update", || {
            let ended = !matches!(old.status, MatchStatus::Ended) && matches!(new.status, MatchStatus::Ended);
            if ended && s.bot_id().is_some_and(|me| i_am_in(ctx, new.id, me)) {
                on_match_ended(ctx, &s, new);
            }
        })
    });

    let s = sh.clone();
    conn.db.auction().on_insert(move |ctx, a| {
        let recv = now_micros();
        touch_rx(&s);
        guard("auction insert", || {
            if !matches!(a.status, AuctionStatus::Open) {
                return;
            }
            let Some(me) = s.bot_id() else { return };
            if !i_am_in(ctx, a.match_id, me) {
                return;
            }
            try_bid(ctx, &s, a, recv);
            // The last auction opening (bag empty) is the cash-out signal even
            // if our rack didn't change.
            try_play_word(ctx, &s, a.match_id);
        })
    });

    let s = sh.clone();
    conn.db.auction_result().on_insert(move |ctx, r| {
        touch_rx(&s);
        guard("auction_result insert", || {
            if is_initial(&ctx.event) {
                return;
            }
            let Some(me) = s.bot_id() else { return };
            if !i_am_in(ctx, r.match_id, me) {
                return;
            }
            let mine = r.winner_bot_id == Some(me);
            let winner = r.winner_bot_id.map_or_else(|| "none".to_string(), |w| bot_name(ctx, w));
            log!(
                "{} auction={} match={} letter={} winner={} paid={}",
                if mine { "WON" } else { "RESULT" },
                r.auction_id,
                r.match_id,
                r.letter,
                winner,
                r.paid
            );
            lock(&s.strat).on_auction_result(&AuctionOutcome {
                match_id: r.match_id,
                auction_id: r.auction_id,
                letter: letter_char(&r.letter),
                winner_bot_id: r.winner_bot_id,
                paid: r.paid,
                mine,
            });
        })
    });

    let s = sh.clone();
    conn.db.word_play().on_insert(move |ctx, w| {
        guard("word_play insert", || {
            if is_initial(&ctx.event) {
                return;
            }
            let Some(me) = s.bot_id() else { return };
            if !i_am_in(ctx, w.match_id, me) {
                return;
            }
            let mine = w.bot_id == me;
            log!(
                "WORD match={} bot={}{} word={} base={} bonus={} reward={}",
                w.match_id,
                bot_name(ctx, w.bot_id),
                if mine { "(me)" } else { "" },
                w.word,
                w.base_score,
                w.bonus,
                w.total_reward
            );
            lock(&s.strat).on_word_played(&WordPlayed {
                match_id: w.match_id,
                bot_id: w.bot_id,
                word: w.word.clone(),
                total_reward: w.total_reward,
                mine,
            });
        })
    });

    let s = sh.clone();
    conn.db.my_rack().on_insert(move |ctx, h| guard("my_rack insert", || try_play_word(ctx, &s, h.match_id)));
    let s = sh.clone();
    conn.db.my_rack().on_update(move |ctx, _old, h| guard("my_rack update", || try_play_word(ctx, &s, h.match_id)));
    let s = sh.clone();
    conn.db.my_rack().on_delete(move |ctx, h| guard("my_rack delete", || try_play_word(ctx, &s, h.match_id)));

    let s = sh.clone();
    conn.db.open_lobby_members().on_insert(move |ctx, m| {
        guard("open_lobby_members insert", || {
            if s.bot_id() == Some(m.bot_id) {
                let lobby = ctx.db().open_lobbies().id().find(&m.lobby_id);
                let members = ctx.db().open_lobby_members().iter().filter(|x| x.lobby_id == m.lobby_id).count();
                log!(
                    "LOBBY_IN lobby={} members={members}/{} closes_in={:.1}s",
                    m.lobby_id,
                    lobby.as_ref().map_or(0, |l| l.max_size),
                    lobby.map_or(f64::NAN, |l| (l.closes_at.to_micros_since_unix_epoch() - now_micros()) as f64 / 1e6)
                );
            }
        })
    });
    let s = sh.clone();
    conn.db.open_lobby_members().on_delete(move |_ctx, m| {
        guard("open_lobby_members delete", || {
            if s.bot_id() == Some(m.bot_id) {
                log!("LOBBY_OUT lobby={}", m.lobby_id);
            }
        })
    });
    let s = sh.clone();
    conn.db.tournament().on_insert(move |ctx, t| {
        guard("tournament insert", || {
            log!("TOURNAMENT id={} status={:?} rounds={} cut={} size={} auction={:?}", t.id, t.status, t.swiss_rounds_total, t.top_cut, t.match_size, t.auction_type);
            maybe_register_tournaments(ctx, &s);
        })
    });
    let s = sh.clone();
    conn.db.tournament().on_update(move |ctx, _old, t| {
        guard("tournament update", || {
            log!("TOURNAMENT id={} status={:?} round={}", t.id, t.status, t.current_round);
            maybe_register_tournaments(ctx, &s);
        })
    });
    let s = sh.clone();
    conn.db.open_lobbies().on_insert(move |ctx, l| {
        guard("open_lobbies insert", || {
            if s.cfg.verbose && !is_initial(&ctx.event) {
                log!("LOBBY_OPENED lobby={} max={} closes_at={}", l.id, l.max_size, fmt_ts(l.closes_at.to_micros_since_unix_epoch()));
            }
        })
    });
}

// ---------- connection loop ----------

enum Outcome {
    Retry(String),
    Done,
    Fatal(String),
}

fn reset_connection_state(sh: &Shared) {
    let mut st = lock(&sh.st);
    st.scope_requested = false;
    st.ready = false;
    st.connected_at = Some(Instant::now());
    st.lobby_inflight = false;
    st.last_lobby_try = None;
    st.words_inflight.clear();
    st.claim_inflight = false;
    // Handles of the old connection's subscriptions; they ended with it.
    st.scope = None;
    st.old_scopes.clear();
    st.last_rx = None;
}

fn touch_rx(sh: &Shared) {
    lock(&sh.st).last_rx = Some(Instant::now());
}

fn join_quietly(handle: thread::JoinHandle<()>, wait: Duration) {
    let deadline = Instant::now() + wait;
    while !handle.is_finished() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if handle.is_finished() && handle.join().is_err() {
        log!("CONNECTION_THREAD_PANICKED");
    }
}

fn run_connection(sh: &Arc<Shared>) -> Outcome {
    let (tx, rx) = mpsc::channel::<Signal>();
    *lock(&sh.signal) = Some(tx.clone());
    reset_connection_state(sh);
    let token = lock(&sh.st).token.clone().or_else(|| sh.cfg.token.clone());

    let sh_conn = sh.clone();
    let (tx_err, tx_disc) = (tx.clone(), tx.clone());
    let built = DbConnection::builder()
        .with_uri(sh.cfg.host.as_str())
        .with_database_name(sh.cfg.db.clone())
        .with_token(token)
        .on_connect(move |conn, identity, token| guard("on_connect", || on_connect(conn, &sh_conn, identity, token)))
        .on_connect_error(move |_ctx, err| {
            let _ = tx_err.send(Signal::Disconnected(format!("connect error: {err}")));
        })
        .on_disconnect(move |_ctx, err| {
            let why = err.map_or_else(|| "closed".to_string(), |e| e.to_string());
            let _ = tx_disc.send(Signal::Disconnected(format!("disconnected: {why}")));
        })
        .build();
    let conn = match built {
        Ok(conn) => conn,
        // The server rejected the token itself; retrying can't help.
        Err(e) if e.to_string().contains("401 Unauthorized") => {
            return Outcome::Fatal(format!("token rejected by {}: {e}", sh.cfg.host))
        }
        Err(e) => return Outcome::Retry(format!("connect failed: {e}")),
    };
    register_callbacks(&conn, sh);
    let handle = conn.run_threaded();

    loop {
        match rx.recv_timeout(Duration::from_secs(1)) {
            Ok(Signal::Done) => {
                let _ = conn.disconnect();
                join_quietly(handle, Duration::from_secs(3));
                return Outcome::Done;
            }
            Ok(Signal::Fatal(msg)) => {
                let _ = conn.disconnect();
                join_quietly(handle, Duration::from_secs(3));
                return Outcome::Fatal(msg);
            }
            Ok(Signal::Disconnected(why)) => {
                let _ = conn.disconnect();
                join_quietly(handle, Duration::from_secs(3));
                return Outcome::Retry(why);
            }
            Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => {
                if handle.is_finished() || !conn.is_active() {
                    join_quietly(handle, Duration::from_secs(1));
                    return Outcome::Retry("connection thread stopped".into());
                }
                let (ready, since) = {
                    let st = lock(&sh.st);
                    (st.ready, st.connected_at.map(|t| t.elapsed()))
                };
                if !ready && since.is_some_and(|d| d > Duration::from_secs(30)) {
                    let _ = conn.disconnect();
                    join_quietly(handle, Duration::from_secs(3));
                    return Outcome::Retry("subscriptions not ready after 30s".into());
                }
                // A running match with no events for a few seconds: the
                // connection has stalled (seen live: 60 s of silence, then a
                // close). Reconnect now rather than lose the rest of the match.
                let stalled = {
                    let st = lock(&sh.st);
                    st.ready && st.last_rx.is_some_and(|t| t.elapsed() > Duration::from_millis(3500))
                };
                if stalled {
                    if let Some(me) = sh.bot_id() {
                        if !my_running_matches(&conn, me).is_empty() {
                            log!("STALL no match events for 3.5s mid-match; reconnecting");
                            let _ = conn.disconnect();
                            join_quietly(handle, Duration::from_secs(2));
                            return Outcome::Retry("stalled connection".into());
                        }
                    }
                }
                // The module keeps one `connected` flag per identity. A stale
                // connection the server reaps late (after we've reconnected)
                // sets it to false while we're online, and the website then
                // shows the bot offline. A fresh connect sets it again: do that
                // between matches, at most once a minute.
                let (conn_age, last_refresh, ident) = {
                    let st = lock(&sh.st);
                    (
                        if st.ready { st.connected_at.map(|t| t.elapsed()) } else { None },
                        st.last_flag_refresh,
                        st.identity,
                    )
                };
                if conn_age.is_some_and(|d| d > Duration::from_secs(5))
                    && last_refresh.is_none_or(|t| t.elapsed() > Duration::from_secs(60))
                {
                    let off = ident
                        .and_then(|id| conn.db.bot_credential().identity().find(&id))
                        .is_some_and(|c| !c.connected);
                    let in_match = sh.bot_id().is_some_and(|me| !my_running_matches(&conn, me).is_empty());
                    if off && !in_match {
                        lock(&sh.st).last_flag_refresh = Some(Instant::now());
                        log!("CRED_FLAG_OFF server shows us offline while connected; reconnecting to refresh it");
                        let _ = conn.disconnect();
                        join_quietly(handle, Duration::from_secs(2));
                        // Let the server process our close before the new connect.
                        thread::sleep(Duration::from_millis(1500));
                        return Outcome::Retry("refresh connected flag".into());
                    }
                }
                // Covers a cancelled lobby, a join that bounced, a missed end.
                guard("watchdog", || maybe_join_lobby(&conn, sh, "watchdog"));
            }
        }
    }
}

fn main() {
    if std::env::args().any(|a| a == "-h" || a == "--help") {
        let doc: Vec<&str> = include_str!("main.rs")
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .map(|l| l.trim_start_matches("//!").strip_prefix(' ').unwrap_or(""))
            .collect();
        println!("{}", doc.join("\n"));
        return;
    }
    let cfg = match Config::from_env() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("zcrabble-bot: {e}");
            std::process::exit(2);
        }
    };
    *lock(&LABEL) = cfg.name.clone().unwrap_or_else(|| "bot".into());
    let t0 = Instant::now();
    let dict: &'static Dictionary = Box::leak(Box::new(Dictionary::load()));
    let strategy = strategy_stub::make_strategy(dict, cfg.min_word_len);
    log!(
        "START host={} db={} dict_words={} dict_load_ms={} strategy={} max_matches={:?} token={}",
        cfg.host,
        cfg.db,
        dict.len(),
        t0.elapsed().as_millis(),
        strategy.name(),
        cfg.max_matches,
        if cfg.token.is_some() { "yes" } else { "none (BOT_NONCE claim)" }
    );
    let sh = Arc::new(Shared {
        cfg,
        dict,
        st: Mutex::new(State::default()),
        strat: Mutex::new(strategy),
        signal: Mutex::new(None),
    });

    // Auctions last a second, so retry often; the cap keeps a long outage cheap.
    const FIRST_RETRY: Duration = Duration::from_millis(500);
    let mut backoff = FIRST_RETRY;
    loop {
        let started = Instant::now();
        match run_connection(&sh) {
            Outcome::Done => {
                let st = lock(&sh.st);
                if !st.lat_all.is_empty() {
                    log!("LATENCY total {}", lat_summary(&st.lat_all));
                }
                log!("EXIT matches_finished={}", st.matches_finished);
                return;
            }
            Outcome::Fatal(msg) => {
                log!("FATAL {msg}");
                std::process::exit(2);
            }
            Outcome::Retry(why) => {
                if started.elapsed() > Duration::from_secs(30) {
                    backoff = FIRST_RETRY;
                }
                log!("RECONNECT in {:.1}s: {why}", backoff.as_secs_f64());
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(3));
            }
        }
    }
}
