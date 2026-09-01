<!--Reference: https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html-->
# Architecture


## Overview

Unified monitoring daemon for social platforms. Watches Discord, Telegram, Twitter, YouTube, and Gmail for relevant events, routes notifications through Telegram.

The repository is a Cargo workspace with three members:

- `social_networks` — the binary crate. Thin CLI dispatcher.
- `social_networks_adapters` — long-running surface adapters. Each adapter implements the `Client` trait.
- `social_networks_utils` — shared primitives (config, db, telegram notifier/utils, misc utils).

## Codemap

```
social_networks/
├── Cargo.toml                              # workspace root
│
├── social_networks/                        # binary crate
│   └── src/
│       ├── main.rs                         # CLI entry, command dispatch
│       ├── config.rs                       # root config + LiveSettings
│       ├── dms.rs                          # notification rules over the DM event stream
│       ├── health.rs                       # service/config/disk health checks
│       └── rolodex/                        # per-person Nix files from Discord, Telegram, GitHub, LinkedIn
│
├── social_networks_adapters/               # long-running surface adapters
│   └── src/
│       ├── lib.rs
│       ├── client.rs                       # `Client` trait, `AdapterError`, `alert()`
│       ├── discord.rs                      # WebSocket gateway, close-frame classification
│       ├── telegram_dms.rs                 # MTProto DM monitoring
│       ├── telegram_channel_watch.rs       # Channel forwarding with keyword filtering
│       ├── twitter.rs                      # Poll monitoring from Twitter lists
│       ├── twitter_schedule.rs             # Scheduled poll posting (OAuth 1.0a)
│       ├── email.rs                        # Gmail IMAP/OAuth, LLM classification
│       └── youtube.rs                      # RSS monitoring, sentiment analysis
│
└── social_networks_utils/                  # shared primitives
    └── src/
        ├── lib.rs
        ├── db.rs                           # SQLite client (libsql): email dedup, rolodex checkpoints
        ├── telegram_notifier.rs            # central notification hub
        ├── telegram_utils.rs               # shared MTProto connect helpers
        └── utils.rs                        # BTC price fetch, number formatting
```

## The `Client` trait

```rust
#[trait_variant::make(Send)]
pub trait Client {
    fn surface(&self) -> &'static str;
    async fn listen(&mut self) -> Result<Infallible, AdapterError>;
}
```

`listen` runs forever in the happy path and only returns on an error class the adapter does not know how to recover from in-process. Recoverable errors (network blips, transient HTTP, known retriable RPC codes) are handled internally with backoff. Anything that escapes is treated as terminal: the binary calls `alert()` (shells out to `v_notify`) and exits non-zero.

`AdapterError` has two variants:
- `Auth { surface, detail }` — credentials are no longer valid. Retrying cannot help.
- `Unhandled { surface, detail }` — an error class the adapter has not classified as recoverable. Treated the same as `Auth` (alert + exit) by policy.

### Per-adapter classification

| Surface | Recoverable inside `listen` | `AdapterError::Auth` |
|---|---|---|
| Discord DMs | network errors, codes 1000-1011, 4000-4003, 4005-4009 | **4004, 4010, 4011, 4012, 4013, 4014** |
| Telegram DMs / channel watch | network errors, generic RPC failures, runner exit | RPC `AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`, `USER_DEACTIVATED`, `AUTH_KEY_INVALID`, `API_ID_INVALID`, `PHONE_NUMBER_BANNED` |
| Twitter monitor / schedule | 429, 5xx, network errors | **401, 403** |
| Email (IMAP + OAuth) | network errors, transient IMAP errors | IMAP login failure; OAuth refresh 401/403 |
| YouTube | 429, 5xx | 401/403 |

## Data Flow

```
Discord ──┐                              ┌── Alerts Channel (pings, monitored users)
Telegram ─┤                              │
Twitter ──┼──► TelegramNotifier ─────────┤
YouTube ──┤                              │
Gmail ────┘                              └── Output Channel (polls, videos, emails)

When an adapter's `listen()` returns an error:
  AdapterError ──► v_notify (high-importance Telegram alert) ──► process exits non-zero
```

`rolodex` is the one surface command that is not a daemon and notifies nobody — it reads the same
sessions on demand and writes to disk, and is the only place anything goes *out* over them:

```
Discord ──┐                                                    ┌──► Discord
Telegram ─┼──► pull ──► LLM extraction ──► <person>.nix    dm ─┼──► Telegram
GitHub ───┤                                                    └──► Twitter
LinkedIn ─┘
```

## Key Entities

- `AppConfig` (bin::config): root config with per-service sections. Wrapped in `LiveSettings` for update awareness.
- `TelegramNotifier` (utils::telegram_notifier): all in-band outbound notifications flow through here.
- `Database` (utils::db): SQLite (libsql). Email deduplication, and the rolodex per-source cursors that keep a full person-file regeneration from clobbering them.
- `Client` / `AdapterError` (adapters::client): the contract every long-running surface implements.

## Invariants

- **Stack size**: telegram surfaces require 8 MiB stack (vs 2 MiB default) due to deeply nested MTProto types — provisioned in `main.rs` `run_async`.
- **Throttling**: monitored user notifications throttled to 15-minute intervals.
- **Deduplication**: all surfaces track processed items to prevent duplicate notifications.
- **Two-channel routing**: alerts (pings, DMs) vs output (content) are separate Telegram destinations.
- **Auth = exit**: an auth-class failure on any surface alerts via `v_notify` and brings the process down.
- **Provider keys**: carried by `[llm]`, required by the surfaces that reason (youtube, email, `rolodex pull`), refused when empty.

## Cross-Cutting Concerns

- **Error recovery**: adapters loop with backoff on recoverable errors; auth/unknown errors propagate.
- **Out-of-band alerting**: `v_notify` (`alert()` in `client.rs`) is the meta channel — used when surfaces themselves die.
- **State persistence**: JSON files in `~/.local/state/social_networks/`, Telegram sessions in SQLite. Rolodex person files live in a user-chosen directory, everything machine-only stays in the db.
- **LLM integration**: email classification, YouTube sentiment and rolodex extraction go through `ask_llm` at `Model::Slow`, the tier backed by the provider whose key we hold. Another tier means another key in `[llm]`.
- **Systemd deployment**: each command runs as an independent systemd user service.
