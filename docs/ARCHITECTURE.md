<!--Reference: https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html-->
# Architecture


## Overview

Unified monitoring daemon for social platforms. Watches Discord, Telegram, Twitter, YouTube and Gmail for relevant events, routes notifications through Telegram. Alongside it, a hand-run axis that reads the same sessions on demand and writes to disk — which is the whole of how skool and the venues are reached.

The repository is a Cargo workspace with four members:

- `social_networks` — the binary crate. Thin CLI dispatcher, and the commands over a purpose: pull, procure, rank, dm, extraction.
- `social_networks_adapters` — how to talk to a platform, and the only place that knows. Daemons implement `Client`; the on-demand axis implements `Profiles` / `Direct` / `Venue`.
- `social_networks_reach` — the transcript format and its store, the purposes over it (people, typed tags, the ranking formula), plus `recon`, the CLI over the venue axis.
- `social_networks_utils` — shared primitives (db, telegram notifier/utils, image conversion, misc utils).

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
│       └── purpose/                        # the commands over a purpose; `rolodex` is `purpose rolodex`
│
├── social_networks_adapters/               # how to talk to a platform
│   └── src/
│       ├── lib.rs
│       ├── client.rs                       # `Client` trait, `AdapterError`, `alert()`  — the daemon axis
│       ├── reach.rs                        # `Profiles`/`Direct`/`Venue` + `Item`       — the on-demand axis
│       ├── discord.rs                      # WebSocket gateway, close-frame classification; REST reads and sends
│       ├── telegram_dms.rs                 # MTProto DM monitoring; peers, dialogs, participants
│       ├── telegram_channel_watch.rs       # Channel forwarding with keyword filtering
│       ├── twitter.rs                      # Poll monitoring from Twitter lists; outbound DMs
│       ├── twitter_schedule.rs             # Scheduled poll posting (OAuth 1.0a)
│       ├── email.rs                        # Gmail IMAP/OAuth, LLM classification
│       ├── github.rs                       # public event feeds, org/repo rosters
│       ├── linkedin.rs                     # logged-out profile reads, behind a refresh queue
│       ├── telegram_notifier.rs            # central notification hub
│       ├── skool.rs                        # `__NEXT_DATA__` reads (feed, roster, classroom), chat writes, browser-minted cookie; the chat poller
│       └── youtube.rs                      # RSS monitoring, sentiment analysis; yt-dlp reads of a channel or a video, on demand (feature `youtube-reads`)
│
├── social_networks_reach/                  # the transcript format and its store
│   ├── src/
│   │   ├── lib.rs                          # the telegram session wrapper
│   │   ├── history.rs                      # `<person>/<year>.md`, cursors, the backfill's two states
│   │   ├── venue.rs                        # `<venues>/<platform>/<slug>/`, the line reader, roster selection
│   │   ├── person.rs                       # `<person>/__main__.nix`, typed tag values
│   │   ├── purpose.rs                      # `purposes.<name>`: folder, tag vocabulary, procurement, ranking — checked at load
│   │   ├── rank.rs                         # the one ranking formula, and the log-age recency axis
│   │   └── recon.rs                        # the venue axis, hand-run
│   └── tests/                              # the ranking's invariants over `examples/purposes/reviews.nix`
│
└── social_networks_utils/                  # shared primitives
    └── src/
        ├── lib.rs
        ├── avif.rs                         # attachment images, kept at an archive's size
        ├── db.rs                           # SQLite client (libsql): email dedup
        ├── telegram_utils.rs               # shared MTProto connect helpers
        └── utils.rs                        # BTC price fetch, number formatting
```

## Two axes

A platform is reached in one of two ways, and the seam between them is which side starts.

```
   Client       listen() forever ─► DmEvent / notification      a daemon, always on
   reach        profile / direct / venues / members / posts     asked, and only by a human
```

A platform may sit on both, and skool does: it is read on demand, and its chat is *polled*, so a
`/ping` there is not something you find out about tomorrow.

`Client` is below; [`reach`](../social_networks_adapters/src/reach.rs) is the **thin waist**: three
traits, six methods, and one `Item` that carries its own author — so a DM, a group post and a public
event differ in `Kind` and in nothing else. Everything a platform does lives behind it, and nothing
above it names a platform except to dispatch.

```
                        person ─► Profiles::profile ─┐
                               ─► Direct::direct  ───┤
                               ─► Direct::send       │
                        venue  ─► Venue::venues      ├─► Item ─► <year>.md
                               ─► Venue::members ────┤
                               ─► Venue::posts   ────┘
```

Dispatch is an exhaustive `match` over `Source` (the person axis) and `VenueSource` (the venue axis)
rather than over `dyn`: a platform that grows an axis is a variant nothing compiles without handling,
where a trait object would have let it fall through to an arm that fetches nothing.

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
| Skool chat | any refused poll, up to 5 in a row | — a dead cookie is re-minted in-process |

## Data Flow

```
Discord ──┐                              ┌── Alerts Channel (pings, monitored users)
Telegram ─┤                              │
Skool ────┤                              │
Twitter ──┼──► TelegramNotifier ─────────┤
YouTube ──┤                              │
Gmail ────┘                              └── Output Channel (polls, videos, emails)

When an adapter's `listen()` returns an error:
  AdapterError ──► v_notify (high-importance Telegram alert) ──► process exits non-zero
```

`purpose` (and `rolodex`, which is `purpose rolodex`) and `recon` are the commands that are not daemons and notify nobody — they read the same
sessions on demand and write to disk, and `dm` is the only place anything goes *out* over them:

```
Discord ──┐                                                                  ┌──► Discord
Telegram ─┤              ┌─► history ────────► <purpose>/<person>/<year>.md     ├──► Skool
GitHub ───┼──► pull ─────┤                                                  dm ─┼──► Telegram
LinkedIn ─┤              └─► LLM extraction ─► <purpose>/<person>/__main__.nix  └──► Twitter
Skool ────┘                         ▲                    │
                                    │                    └──► rank ◄── tags + year files + venue lines
                                    │ lines matching `[<handle>/`
Telegram ─┐   members ──────────────┼──► <venues>/<platform>/<slug>/members.json
GitHub ───┼──► recon                │                                    │
Skool ────┘   posts ────────────────┴──► <venues>/<platform>/<slug>/<year>.md
                                                                         │
                                    procure ◄────────────────────────────┘
                                         └─► a skeleton in the purpose's folder, which `pull` then fills
```

The transcript is what a read is for; the labels in `__main__.nix` are derived from it and can be
regenerated from it. A venue transcript keeps the whole conversation, including people nobody tracks
— a thread with the non-members cut out is not the thread — and none of it is copied into a person's
file, which stays their DMs. A person's own lines are selected out of it at `pull` time by the prefix
the writer put there, so nothing is derived that could not be rebuilt.

Skool is the platform that shapes the most around it, because it publishes no API and reaches nobody
outside a shared group. What that costs, and why a browser sits on the login path and nowhere else,
is on [`adapters::skool`](../social_networks_adapters/src/skool.rs).

## Key Entities

- `AppConfig` (bin::config): root config with per-service sections. Wrapped in `LiveSettings` for update awareness.
- `TelegramNotifier` (adapters::telegram_notifier): all in-band outbound notifications flow through here.
- `Database` (utils::db): SQLite (libsql). Email deduplication.
- `Client` / `AdapterError` (adapters::client): the contract every long-running surface implements.
- `Profiles` / `Direct` / `Venue` / `Item` (adapters::reach): the contract every on-demand read goes through.
- `Purpose` (reach::purpose): what the people in one folder are *for* — its tag vocabulary, its procurement strategies, its ranking terms. Every writer of a tag goes through `Purpose::check`.
- `Person` (reach::person): a person directory's `__main__.nix`, tags typed against their purpose.
- `rank` (reach::rank): `Σ w·v / Σ w` over terms in `[0,1]`; the builtins are derived from the transcripts at rank time, never stored.

## Invariants

- **Stack size**: telegram surfaces require 8 MiB stack (vs 2 MiB default) due to deeply nested MTProto types — provisioned in `main.rs` `run_async`.
- **Throttling**: monitored user notifications throttled to 15-minute intervals.
- **Deduplication**: all surfaces track processed items to prevent duplicate notifications.
- **Two-channel routing**: alerts (pings, DMs) vs output (content) are separate Telegram destinations.
- **Auth = exit**: an auth-class failure on any surface alerts via `v_notify` and brings the process down.
- **Provider keys**: carried by `[llm]`, required by the surfaces that reason (youtube, email, a purpose's `pull`), refused when empty.
- **One place per platform**: everything that knows a platform's endpoints, payloads and auth lives in `social_networks_adapters` and nowhere else. The waist is the only seam.
- **The transcript is the artifact**: a person's and a venue's year files are what a read is for. Nothing is derived from them that cannot be rebuilt from them, and there is no index.
- **`recon` is never invoked by a daemon**: rate-limit and account-safety exposure stays human-initiated, which is why it is a binary of `social_networks_reach` rather than a subcommand of the app. `procure` fetches nothing — it selects over what `recon` wrote.
- **A purpose is checked whole at load**: no command holds a purpose whose ranking, procurement or people disagree with its vocabulary.

## Cross-Cutting Concerns

- **Error recovery**: adapters loop with backoff on recoverable errors; auth/unknown errors propagate.
- **Out-of-band alerting**: `v_notify` (`alert()` in `client.rs`) is the meta channel — used when surfaces themselves die.
- **State persistence**: JSON files in `~/.local/state/social_networks/`, Telegram sessions in SQLite. A person's state is co-located with them, under their purpose's folder — a person's messages and cursors are worth as much as the labels over them and are synced with them.
- **LLM integration**: email classification, YouTube sentiment and a purpose's extraction go through `ask_llm` at `Model::Slow`, the tier backed by the provider whose key we hold. Another tier means another key in `[llm]`.
- **Systemd deployment**: each command runs as an independent systemd user service.
