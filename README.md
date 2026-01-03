# social_networks
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/social_networks.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/social_networks)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/social_networks)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/social_networks-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/errors.yml?branch=master&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/warnings.yml?branch=master&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amaster) <!--NB: Won't find it if repo is private-->

scripts to automate scraping from or certain parts of interactions with social networks.

has aggregators of sentiment polls from Twitter and Telegram, interpretation of Hamaha's video titles, discord /ping notifier, etc
<!-- markdownlint-disable -->
<details>
<summary>
<h3>Installation</h3>
</summary>

```sh
cargo install --git https://github.com/valeratrades/social_networks --branch master
```

### Email Setup

The email command supports two authentication methods:

#### Option 1: IMAP with App Password (simpler)
1. Enable 2-Step Verification on your Google account
2. Go to [Google App Passwords](https://myaccount.google.com/apppasswords)
3. Generate an app password for "Mail"
4. Add to your config:
   ```toml
   [email]
   email = "your@gmail.com"
   [email.auth.imap]
   pass = "your-app-password"
   ```

#### Option 2: OAuth (for remote servers)
1. Create a project in [Google Cloud Console](https://console.cloud.google.com/)
2. Enable the Gmail API
3. Create OAuth 2.0 credentials (Desktop app)
4. Add to your config:
   ```toml
   [email]
   email = "your@gmail.com"
   [email.auth.oauth]
   client_id = "..."
   client_secret = "..."
   ```
5. Run `social_networks email` on your **local machine** (where you can open a browser)
6. Complete the OAuth authentication flow
7. Copy the token file to the remote server:
   ```sh
   scp ~/.local/state/social_networks/gmail_tokens.json <remote-server>:~/.local/state/social_networks/
   ```

</details>
<!-- markdownlint-restore -->

## Usage
Fill in `~/.config/social_networks.toml` following [examples/config.toml](./examples/config.toml).

### Commands

| Command | Description |
|---------|-------------|
| `dms` | DM monitoring (ping, monitored users) for Discord and Telegram simultaneously |
| `email` | Email monitoring with LLM-based filtering (forwards human emails to Telegram) |
| `health` | Show health status of all services, config, and directories |
| `migrate-db` | Run database migrations |
| `telegram-channel-watch` | Telegram channel watching (poll/info forwarding) |
| `twitter` | Twitter operations |
| `twitter-schedule` | Twitter scheduled posting |
| `youtube` | YouTube operations |



<br>

<sup>
	This repository follows <a href="https://github.com/valeratrades/.github/tree/master/best_practices">my best practices</a> and <a href="https://github.com/tigerbeetle/tigerbeetle/blob/main/docs/TIGER_STYLE.md">Tiger Style</a> (except "proper capitalization for acronyms": (VsrState, not VSRState) and formatting). For project's architecture, see <a href="./docs/ARCHITECTURE.md">ARCHITECTURE.md</a>.
</sup>

#### License

<sup>
	Licensed under <a href="LICENSE">GLWTS</a>
</sup>

<br>

<sub>
	Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be licensed as above, without any additional terms or conditions.
</sub>

