# Changelog

## Unreleased

- **Workspace split.** Repo is now a Cargo workspace with three members: `social_networks` (binary), `social_networks_adapters` (long-running surface adapters), and `social_networks_utils` (shared primitives — config, db, telegram notifier/utils).
- **Shared `Client` trait.** Every long-running surface (Discord, Telegram DMs, Telegram channel watch, Twitter monitor, Twitter schedule, Email, YouTube) now implements `social_networks_adapters::Client` with a single `listen()` method returning `Result<Infallible, AdapterError>`.
- **Behavior change: auth errors exit and alert.** Recoverable errors (network blips, 429, 5xx, transient RPC failures) are still retried inside each adapter with backoff. Auth-class errors (Discord WS codes 4004/4010-4014; Telegram `AUTH_KEY_UNREGISTERED`/`SESSION_REVOKED`/`USER_DEACTIVATED`/`AUTH_KEY_INVALID`/`API_ID_INVALID`/`PHONE_NUMBER_BANNED`; HTTP 401/403; IMAP login failures) and any unclassified error now propagate, fire a high-importance Telegram alert via `v_notify`, and exit the process non-zero.
- **Discord close-frame classification.** Previously the `Message::Close` frame from `tokio_tungstenite` was silently swallowed and the adapter would reconnect forever. Now the close code is logged, fatal codes (4004/4010-4014) trigger the alert + exit path, and other codes reconnect with backoff.
- **Removed.** Deleted unused `src/twitter_user.rs` (was `#[deprecated]`); collapsed `src/dms/mod.rs` orchestrator into `main.rs`'s `Dms` arm via `tokio::select!`.
