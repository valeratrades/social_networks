Fill in `~/.config/social_networks.nix`. Follow [examples/config.nix](../../examples/config.nix).

## Commands

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

### `rolodex`

| Command | Description |
|---------|-------------|
| `rolodex open [pattern]` | Open a person file in `$EDITOR`. Create the file if the pattern finds nobody. |
| `rolodex pull [pattern]` | Get new data for each person the pattern finds. Write it to their files. |
| `rolodex discover <platform>:<slug>` | Make a file for each member of a group that has no file yet. |
| `rolodex dm <--platform> <pattern> <text>` | Send one message to one person. |

A pattern finds a person by file name or by any handle. Without a pattern, `open` starts `fzf` and
`pull` takes everybody.

`pull` also keeps the messages. It writes them to `<person>/<year>.md`, next to the person file. The
first `pull` gets the full history of each conversation, and can take a long time. If you stop it,
the next `pull` continues from the same place.

### `recon`

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
