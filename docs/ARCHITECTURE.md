<!--Reference: https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html-->
# Architecture


## Overview

Unified monitoring daemon for social platforms. Watches Discord, Telegram, Twitter, YouTube and Gmail for relevant events, routes notifications through Telegram. Alongside it, a hand-run axis that reads the same sessions on demand and writes to disk — which is the whole of how skool and the venues are reached.

The repository is a Cargo workspace with four members:

- `social_networks` — the binary crate. Thin CLI dispatcher, and the commands over a purpose: pull, procure, rank, dm, extraction.
- `social_networks_adapters` — how to talk to a platform, and the only place that knows. Daemons implement `Client`; the on-demand axis implements `Profiles` / `Direct` / `Venue`, and a session driving a browser by hand `Browsing`.
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
│       ├── health.rs                       # config/disk checks, hand-run
│       └── purpose/                        # the commands over a purpose; `rolodex` is `purpose rolodex`
│           └── dm.rs                       # the one send path; writes what went out into the transcript where no pull reads it back
│
├── social_networks_adapters/               # how to talk to a platform
│   └── src/
│       ├── lib.rs
│       ├── client.rs                       # `Client` trait, `AdapterError`, `alert()`  — the daemon axis
│       ├── reach.rs                        # `Profiles`/`Direct`/`Venue`/`Browsing` + `Item` — the on-demand axis
│       ├── breaker.rs                      # circuit breakers every send to a person is admitted through
│       ├── behaviour.rs                    # how a paced session spends its time: active hours, bursts, caps, dwell; a banded shuffle
│       ├── discord.rs                      # WebSocket gateway, close-frame classification; REST reads and sends
│       ├── telegram_dms.rs                 # MTProto DM monitoring; peers, dialogs, participants
│       ├── telegram_channel_watch.rs       # Channel forwarding with keyword filtering, on its own session
│       ├── twitter.rs                      # Poll monitoring from Twitter lists; outbound DMs
│       ├── twitter_schedule.rs             # Scheduled poll posting (OAuth 1.0a)
│       ├── email/                          # Gmail IMAP/OAuth, thread reads, LLM classification; `script.rs`: conversations it answers on its own
│       ├── facebook/                       # a logged-in chrome through `browser_manipulation`: City-filter search, group listings, About-tab visits; Messenger sends; `noise.rs`: idle browsing after them
│       ├── nominatim.rs                    # place name → point, ≤1 req/s, cached on disk forever
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
│   │   ├── outbox.rs                       # `<person>/outbox/<messenger>/<at>.md`: messages written ahead, sent by `purpose send`; `--multi-message` sends one as a burst of bubbles split on blank lines, one breaker admission; a burst cut short leaves only its unsent bubbles in the file
│   │   ├── venue.rs                        # `<venues>/<platform>/<slug>/`, the line reader, roster selection
│   │   ├── person.rs                       # `<person>/__main__.nix`, typed tag values
│   │   ├── purpose.rs                      # `purposes.<name>`: folder, tag vocabulary, procurement, ranking — checked at load
│   │   ├── rank.rs                         # the one ranking formula, the log-age recency axis, the decay over a lead whose last line is ours
│   │   └── recon.rs                        # the venue axis, hand-run
│   └── tests/                              # the ranking's invariants over `examples/purposes/reviews.nix`; groups; facts
│
└── social_networks_utils/                  # shared primitives
    └── src/
        ├── lib.rs
        ├── avif.rs                         # attachment images, kept at an archive's size
        ├── db.rs                           # SQLite client (libsql): email dedup, twitter_schedule attempts, daemon state, telegram sessions
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

`Client` is below; [`reach`](../social_networks_adapters/src/reach.rs) is the **thin waist**: four
traits, seven methods, a `Roster` a listing is checked into, and one `Item` that carries its own author — so a DM, a group post and a public
event differ in `Kind` and in nothing else. Everything a platform does lives behind it, and nothing
above it names a platform except to dispatch.

```
                        person ─► Profiles::profile ─┐
                               ─► Direct::direct  ───┤
                               ─► Direct::send       │
                        venue  ─► Venue::venues      ├─► Item ─► <year>.md
                               ─► Venue::posts   ────┘
                               ─► Venue::members ──────► Roster ─► members.json, page by page
                        session ─► Browsing::noise         nothing; idle browsing after a send
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

`listen` runs forever in the happy path and only returns on an error class the adapter does not know how to recover from in-process. Recoverable errors (network blips, transient HTTP, known retriable RPC codes) are handled internally with backoff. Anything that escapes is treated as terminal: the binary calls `alert()` and exits non-zero.

`AdapterError` has two variants:
- `Auth { surface, detail }` — credentials are no longer valid. Retrying cannot help.
- `Unhandled { surface, detail }` — an error class the adapter has not classified as recoverable. Treated the same as `Auth` (alert + exit) by policy.

### Per-adapter classification

| Surface | Recoverable inside `listen` | `AdapterError::Auth` |
|---|---|---|
| Discord DMs | network errors, codes 1000-1011, 4000-4003, 4005-4009 | **4004, 4010, 4011, 4012, 4013, 4014** |
| Telegram DMs / channel watch | network errors, generic RPC failures, runner exit | RPC `AUTH_KEY_UNREGISTERED`, `SESSION_REVOKED`, `USER_DEACTIVATED`, `AUTH_KEY_INVALID`, `API_ID_INVALID`, `PHONE_NUMBER_BANNED` |
| Twitter monitor / schedule | 429, 5xx, network errors | **401, 403** |
| Email (IMAP + OAuth) | network errors, transient IMAP errors | IMAP login failure; SMTP login failure; OAuth refresh 401/403 |
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
  AdapterError ──► error! (+ OTLP flush) ──► process exits non-zero ──► pod crashloop ──► tenant-health alert
```

The email daemon is the one daemon that writes back: a thread its account's `scripts` key opened is
answered by the LLM toward the script's goal, until it answers that the goal is reached, which goes
to the alerts channel instead. `--dry-run` sends the drafts there too.
`--delay <a>..=<b>` holds each reply for a span drawn from the range, in memory: a restart decides
its message afresh.

`purpose` (and `rolodex`, which is `purpose rolodex`) and `recon` are the commands that are not daemons and notify nobody — they read the same
sessions on demand and write to disk, and `dm` is the only place anything goes *out* over them (`send` is `dm` over what is due in the outboxes):

```
Discord ──┐                                                                  ┌──► Discord
Telegram ─┤              ┌─► history ────────► <purpose>/<person>/<year>.md     ├──► Facebook
GitHub ───┤              │                                                  dm ─┼──► Skool
LinkedIn ─┼──► pull ─────┼─► LLM extraction ─► <purpose>/<person>/__main__.nix  ├──► Telegram
Skool ────┤              │                         ▲     │                      └──► Twitter
Facebook ─┘              └─► facts (lives_in) ─────┘     └──► rank ◄── tags + year files + venue lines
                                    ▲
                                    │ lines matching `[<handle>/`
Telegram ─┐   members ──────────────┼──► <venues>/<platform>/<slug>/members.json
GitHub ───┤                         │                                    │
Skool ────┼──► recon                │                                    │
Facebook ─┘   posts ────────────────┴──► <venues>/<platform>/<slug>/<year>.md
                                                                         │
                                    procure ◄────────────────────────────┘
                                         └─► a skeleton in the purpose's folder, which `pull` then fills;
                                             a row that places somebody seeds its `lives_in`
```

The transcript is what a read is for; the labels in `__main__.nix` are derived from it and can be
regenerated from it. A venue transcript keeps the whole conversation, including people nobody tracks
— a thread with the non-members cut out is not the thread — and none of it is copied into a person's
file, which stays their DMs. A person's own lines are selected out of it at `pull` time by the prefix
the writer put there, so nothing is derived that could not be rebuilt.

Facebook is read through a logged-in chrome and nothing else, over two sessions that never mix: the
user's own (attached over CDP) walks the City-filter people search, and our own headless chrome on a
burner account lists groups and visits profiles. Messages go out over Messenger from the burner, or
from a third chrome on an account of its own when `facebook.send` is set. A city walk takes days, so a roster is checked in
page by page with a resume cursor beside it rather than returned whole. See
[`adapters::facebook`](../social_networks_adapters/src/facebook/mod.rs) and `docs/facebook/`.

Skool is the platform that shapes the most around it, because it publishes no API and reaches nobody
outside a shared group. What that costs, and why a browser sits on the login path and nowhere else,
is on [`adapters::skool`](../social_networks_adapters/src/skool.rs).

## Ranking

```
score = Σwv/Σw  ×  (1 − 2^(−Δt / unanswered_half_life))    only while their newest line is ours
         └ terms ┘   └──── on the whole result ────┘
         just sent ×0 · 1w ×½ · 2w ×¾ · 4w ×15/16 · they reply → ×1
```

## Key Entities

- `AppConfig` (bin::config): root config with per-service sections. Wrapped in `LiveSettings` for update awareness.
- `TelegramNotifier` (adapters::telegram_notifier): all in-band outbound notifications flow through here.
- `Database` (utils::db): SQLite (libsql). Email deduplication; twitter_schedule attempts, which a restart schedules from; the daemons' cursors and caches; telegram sessions, through `DbSession`.
- `Client` / `AdapterError` (adapters::client): the contract every long-running surface implements.
- `Profiles` / `Direct` / `Venue` / `Item` (adapters::reach): the contract every on-demand read goes through.
- `Purpose` (reach::purpose): what the people in one folder are *for* — its tag vocabulary, its procurement strategies, its ranking terms. Every writer of a tag goes through `Purpose::check`.
- `Person` (reach::person): a person directory's `__main__.nix`, tags typed against their purpose.
- `rank` (reach::rank): `Σ w·v / Σ w` over terms in `[0,1]`; the builtins are derived from the transcripts at rank time, never stored. Beside the score, `stale`: what a pull stands to move it by, off the purpose's `stale_half_life` and the `fetched_at` / `reasoned_at` in `meta.json`; `pull` walks people by it. The score as a whole is then ×`(1 − 2^(−Δt / unanswered_half_life))` while their newest line is ours.

## Invariants

- **Stack size**: telegram surfaces require 8 MiB stack (vs 2 MiB default) due to deeply nested MTProto types — provisioned in `main.rs` `run_async`.
- **Throttling**: monitored user notifications throttled to 15-minute intervals.
- **Deduplication**: all surfaces track processed items to prevent duplicate notifications.
- **Two-channel routing**: alerts (pings, DMs) vs output (content) are separate Telegram destinations.
- **One telegram auth key, one live connection**: a second concurrent connection on a key — a copied session counts — gets `AUTH_KEY_DUPLICATED` on both ends. So each daemon logs in its own session (`dms` on `<user>`, `telegram-channel-watch` on `<user>_channel_watch`), and no key is shared with another host's process, e.g. the laptop's `tg`.
- **A daemon's state is the db**: sessions, cursors and caches are rows of `db.sqlite3`, the one file the cluster replicates (litestream, devops). A daemon writing a file of its own beside it loses that state on every failover.
- **Auth = exit**: an auth-class failure on any surface brings the process down non-zero. Nothing retries past it in-process; recovery is a human fixing creds and restarting.
- **Provider keys**: carried by `[llm]`, each optional — the `claude` CLI's own login is one — and resolved per request, never at load. A daemon that reasons (youtube, email) exits when no model answers; a purpose's `pull` saves what it fetched and says that nothing reasoned over it.
- **One place per platform**: everything that knows a platform's endpoints, payloads and auth lives in `social_networks_adapters` and nowhere else. The waist is the only seam.
- **The transcript is the artifact**: a person's and a venue's year files are what a read is for. Nothing is derived from them that cannot be rebuilt from them, and there is no index.
- **`recon` is never invoked by a daemon**: rate-limit and account-safety exposure stays human-initiated, which is why it is a binary of `social_networks_reach` rather than a subcommand of the app. `procure` fetches nothing — it selects over what `recon` wrote.
- **A purpose is checked whole at load**: no command holds a purpose whose ranking, procurement or people disagree with its vocabulary.
- **A fact outranks its seed**: `lives_in` and `birthday` are tags platforms state. `procure` seeds `lives_in` from a roster row that places somebody; a `pull` visit overwrites it, and a visit that finds no current city removes it.
- **A birthday moves only to better evidence**: a stated date over any range of birth years; a newer or narrower range over an older one; undated words only fill a gap. An age is never stored.
- **Facebook**:
  - no `Runtime.enable`: `browser_manipulation`'s invariant 4, held by its patchright driver and tested by its `page_sees_no_automation`;
  - reads click nothing: pages are loaded by URL and read from the JSON they embed and the GraphQL they fetch. A send is the one thing that clicks buttons and types: through the prompts Messenger stands in front of the composer of a conversation loaded by URL, then into the composer, then Enter. Noise (`Browsing`) clicks links and types into the search field, and nothing else: it presses no button, so nothing it does outlives the next load by URL;
  - a message counts as sent only once the conversation shows it; a refusal about the recipient is `Unreachable`, anything else the page says is an error;
  - no request is ours: resuming a city query partway rewrites the `cursor` variable of the page's own next pagination request (`Tab::route`), and nothing else;
  - `city` never launches a browser, and `group`, profile visits and sends never use the user's;
  - messages go out from `facebook.send`'s own chrome profile and state, or from the launched session's when it is unset;
  - credentials are never typed by us: a logged-out attached session waits for a human, a logged-out launched one is an error until `recon facebook-login`;
  - "Lives in" is the only residence signal; "From" (hometown) never counts;
  - pacing is per session, through `behaviour`, whose logs and phase outlive a restart; a browser is opened per command and closed with it, Ctrl-C included;
  - the user's sway focus is never moved: no `Page.bringToFront`, and a window that must render is moved to a headless output, never to the user;
  - the attached window is parked on a headless output only while nobody can see it: focusing its workspace brings it back, and the run ends with it home. A home workspace sway destroyed meanwhile is recreated on the output it was on.
- **Every send to a person is admitted by `breaker`**: `dm`, `send` and the email scripts' replies. A tripped breaker refuses, it never queues; the send log and open breakers are rows of the db.
- **A sent message is on the person's record**, written at send where no pull reads it back: a lead whose newest year-file line is ours has its whole score decayed by the purpose's `unanswered_half_life`.
- **`send` sends to exactly `-n` people**: how many is the human's to say on every run; fewer sendable than asked is an error before anything goes out.
- **Writing a campaign and sending it never share a step**: a writer only drops files into `<person>/outbox/`; `send` reads nothing else, and removes a file only after it went out.
- **Paced sessions**: every adapter `recon` drives against a rate-sensitive platform (facebook's two sessions, skool's group sweeps) goes through one `Behaviour`; a retry backoff answers a block and is not behaviour.

## Cross-Cutting Concerns

- **Error recovery**: adapters loop with backoff on recoverable errors; auth/unknown errors propagate.
- **Out-of-band alerting**: when a surface dies, the error is traced (and, with `OTEL_EXPORTER_OTLP_ENDPOINT` set, flushed to OTLP before exit). `alert()` also shells to `v_notify` where it exists; in the cluster it does not, and the crashloop is what alerts.
- **State persistence**: the daemons' in `~/.local/state/social_networks/db.sqlite3` — telegram sessions (`DbSession`), the `state` table's cursors and the skool cookie; a legacy `<key>.json` or `<name>.session` beside it is imported on first read and removed. The hand-run axis keeps a paced session's logs (`views`, `scrolls`) and `phase.toml` under `facebook/{attached,launched,send}/` and `skool/`; facebook's session drops and our chrome profiles beside them; a failed skool login's page in `skool/browser_captures/`; the city walk's name order seed and its per-query records, `facebook/attached/searched/<city id>/<name>.toml`, the query → account map the roster does not keep. A person's state is co-located with them, under their purpose's folder — a person's messages and cursors are worth as much as the labels over them and are synced with them.
- **LLM integration**: email classification, YouTube sentiment and a purpose's extraction go through `ask_llm` at `Model::Slow`, the tier backed by the provider whose key we hold. Another tier means another key in `[llm]`.
- **Deployment**: one container image (`nix build .#social_networks-container`, pushed to GHCR on tag), one k3s Deployment per daemon subcommand in the `personal` namespace, labelled `app.kubernetes.io/part-of: social-networks`; state and config sit on a shared PVC. The manifests live in `ev_invest/devops` (`daemonDoc`), as does the config (`nix/platform/social_networks.nix`). Auth = exit there reads: the pod crashloops, devops' tenant-health alert fires, the cause is in Loki under `k8s_deployment_name`, and after the creds are fixed the Deployments dashboard's Restart brings it back.
