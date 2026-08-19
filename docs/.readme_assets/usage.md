Fill in `~/.config/social_networks.nix`. Follow [examples/config.nix](../../examples/config.nix).

## Commands

| Command | Description |
|---------|-------------|
| `dms` | DM monitoring (ping, monitored users) for Discord and Telegram simultaneously |
| `email` | Email monitoring with LLM-based filtering (forwards human emails to Telegram) |
| `health` | Show health status of all services, config, and directories |
| `migrate-db` | Run database migrations |
| `rolodex` | Per-person records from Discord, Telegram and GitHub |
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

A pattern finds a person by file name or by any handle. Without a pattern, `open` starts `fzf` and
`pull` takes everybody.
