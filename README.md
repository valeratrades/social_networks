# social_networks
![Minimum Supported Rust Version](https://img.shields.io/badge/nightly-1.92+-ab6000.svg)
[<img alt="crates.io" src="https://img.shields.io/crates/v/social_networks.svg?color=fc8d62&logo=rust" height="20" style=flat-square>](https://crates.io/crates/social_networks)
[<img alt="docs.rs" src="https://img.shields.io/badge/docs.rs-66c2a5?style=for-the-badge&labelColor=555555&logo=docs.rs&style=flat-square" height="20">](https://docs.rs/social_networks)
![Lines Of Code](https://img.shields.io/endpoint?url=https://gist.githubusercontent.com/valeratrades/b48e6f02c61942200e7d1e3eeabf9bcb/raw/social_networks-loc.json)
<br>
[<img alt="ci errors" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/errors.yml?branch=main&style=for-the-badge&style=flat-square&label=errors&labelColor=420d09" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->
[<img alt="ci warnings" src="https://img.shields.io/github/actions/workflow/status/valeratrades/social_networks/warnings.yml?branch=main&style=for-the-badge&style=flat-square&label=warnings&labelColor=d16002" height="20">](https://github.com/valeratrades/social_networks/actions?query=branch%3Amain) <!--NB: Won't find it if repo is private-->

scripts to automate scraping from or certain parts of interactions with social networks.

has aggregators of sentiment polls from Twitter and Telegram, interpretation of Hamaha's video titles, discord /ping notifier, etc
<!-- markdownlint-disable -->
<details>
<summary>
<h2>Installation</h2>
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
Fill in `~/.config/social_networks.nix`. Follow [examples/config.nix](./examples/config.nix).

### Commands

| Command | Description |
|---------|-------------|
| `dms` | DM monitoring (ping, monitored users) for Discord and Telegram simultaneously |
| `email` | Email monitoring with LLM-based filtering (forwards human emails to Telegram) |
| `health` | Show health status of all services, config, and directories |
| `migrate-db` | Run database migrations |
| `rolodex` | Per-person records from Discord, Telegram, GitHub, LinkedIn and Skool |
| `telegram-channel-watch` | Telegram channel watching (poll/info forwarding) |
| `twitter` | Twitter operations |
| `twitter-schedule` | Twitter scheduled posting |
| `youtube` | YouTube operations |

All commands other than `health`, `migrate-db` and `rolodex` run as daemons.

#### `rolodex`

| Command | Description |
|---------|-------------|
| `rolodex open [pattern]` | Open a person file in `$EDITOR`. Create the file if the pattern finds nobody. |
| `rolodex pull [pattern]` | Get new data for each person the pattern finds. Write it to their files. |
| `rolodex discover <platform>:<slug>` | Make a file for each member of a group that has no file yet. |
| `rolodex cold [pattern]` | Show each person that you sent no message to and got no message from. |
| `rolodex dm <--platform> <pattern> <text>` | Send one message to one person. |

A pattern finds a person by file name or by any handle. Without a pattern, `open` starts `fzf` and
`pull` takes everybody.

`pull` also keeps the messages. It writes them to `<person>/<year>.md`, next to the person file. The
first `pull` gets the full history of each conversation, and can take a long time. If you stop it,
the next `pull` continues from the same place.

`cold` finds each person that holds no conversation with you. A line that a person wrote in a group
is not a conversation, so each member that `discover` added stays cold. `cold` checks every platform
that you keep a handle for. It uses the messages that `pull` kept. If `pull` read no messages from a
platform, `cold` asks that platform for one message. It keeps no message that it gets.

#### `recon`

`recon` reads a group, not a person. Run it with
`cargo r -p social_networks_reach --bin recon -- <command>`. It uses the same config file. No daemon
starts it: each command uses part of your rate limit, so you must start it yourself.

| Command | Description |
|---------|-------------|
| `recon venues <platform>` | Show the groups this account can read. |
| `recon members <platform>:<slug>` | Write the member list to `members.json`. |
| `recon posts <platform>:<slug> --since 90d` | Add new posts to the group's `<year>.md` files. |
| `recon roster <platform>:<slug> [--where <sql>]` | Show the member list again. Select part of it with SQL. |

The group files go under `<rolodex path>/venues/<platform>/<slug>/`. `rolodex pull` then reads the
lines of each person you keep a file for. `recon` gets the posts one time, and every `pull` after
that is free.

#### Select members with SQL

`--where` takes a SQL `WHERE` clause. It also takes a path to a file that holds one. `recon roster`
and `rolodex discover` use the same clause and the same table.

| Column | Type | Content |
|--------|------|---------|
| `handle` | text | The name the platform uses. |
| `display` | text | The name the platform prints. |
| `joined` | text | The date the person joined the group. RFC3339. |
| `lat`, `lon` | number | The position the platform gives. |
| `zone` | text | The time zone name, for example `Europe/Berlin`. |
| `posts` | number | The lines in the group transcript that this person wrote. |
| `first_post`, `last_post` | text | The dates of those lines. RFC3339. |

Dates are text. SQLite puts RFC3339 text in date order, so `last_post >= '2026-06-01'` is correct.

Skool gives a position for each member of a group. It moves each position more than 10 miles, to
protect the person. Use a box, because a box is as exact as the data. To get the members in Europe,
with the UK:

```sql
-- ~/rolodex/queries/europe.sql
lat BETWEEN 34 AND 72 AND lon BETWEEN -25 AND 45
```

```
recon members  skool:<group>
recon posts    skool:<group>
rolodex discover skool:<group> --where ~/rolodex/queries/europe.sql --dry-run
rolodex discover skool:<group> --where ~/rolodex/queries/europe.sql
rolodex pull <stem>
```

`discover` writes a file for each selected person who has no file. `pull` then fills each file.

Two limits apply to skool, and both make the member list shorter than the group:

- Skool gives a position only for the members who gave one. In a group of 406, 325 gave one.
- Skool does not page its member list. The page parameter changes the page number in the payload,
  but the payload always holds the first 30 members.

`recon members` uses the group map and the member page together. It reads one page for the map and
one request for each member on it, so it is slow and it waits between requests. Do it one time for
each group. The `zone` column is a second signal, but the person sets it in the browser, so it can
disagree with the position.



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

