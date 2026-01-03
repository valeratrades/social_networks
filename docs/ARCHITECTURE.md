<!--Reference: https://matklad.github.io/2021/02/06/ARCHITECTURE.md.html-->
# Architecture


## Overview

Unified monitoring daemon for social platforms. Watches Discord, Telegram, Twitter, YouTube, and Gmail for relevant events, routes notifications through Telegram.

## Codemap

```
src/
├── main.rs                    # CLI entry, command dispatch
├── config.rs                  # TOML config types, live-reload support
├── health.rs                  # Service/config/disk health checks
├── telegram_notifier.rs       # Central notification hub (alerts vs output channels)
├── db.rs                      # ClickHouse client, email dedup
├── utils.rs                   # BTC price fetch, number formatting
│
├── dms/
│   ├── mod.rs                 # Orchestrates Discord + Telegram monitors concurrently
│   ├── discord.rs             # WebSocket gateway, ping detection, monitored users
│   └── telegram.rs            # MTProto client, session mgmt, DM monitoring
│
├── email.rs                   # Gmail via IMAP/OAuth, LLM classification (human vs bot)
├── twitter.rs                 # Poll monitoring from Twitter lists
├── twitter_schedule.rs        # Scheduled sentiment poll posting (OAuth 1.0a)
├── telegram_channel_watch.rs  # Channel forwarding with keyword filtering
└── youtube.rs                 # RSS monitoring, sentiment analysis on titles
```

## Data Flow

```
Discord ──┐                              ┌── Alerts Channel (pings, monitored users)
Telegram ─┤                              │
Twitter ──┼──► TelegramNotifier ─────────┤
YouTube ──┤                              │
Gmail ────┘                              └── Output Channel (polls, videos, emails)
```

## Key Entities

- `AppConfig` (config.rs): Root config with per-service sections. Wrapped in `LiveSettings`; allowing update awareness.
- `TelegramNotifier` (telegram_notifier.rs): All outbound notifications flow through here
- `Database` (db.rs): Email deduplication via ClickHouse

## Invariants

- **Stack size**: Telegram services require 8MB stack (vs 2MB default) due to deeply nested MTProto types
- **Throttling**: Monitored user notifications throttled to 15-minute intervals
- **Deduplication**: All services track processed items to prevent duplicate notifications
- **Two-channel routing**: Alerts (pings, DMs) vs Output (content) are separate Telegram destinations

## Cross-Cutting Concerns

- **Error recovery**: Services loop forever with exponential backoff on errors
- **State persistence**: JSON files in `~/.local/state/social_networks/`, Telegram sessions in SQLite
- **LLM integration**: Email classification and YouTube sentiment via Claude (ask_llm crate)
- **Systemd deployment**: Each command designed to run as independent systemd user service
