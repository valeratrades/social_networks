Fill in `~/.config/social_networks.nix`. Follow [examples/config.nix](../../examples/config.nix).

## Commands

| Command | Description |
|---------|-------------|
| `dms` | DM monitoring (ping, monitored users) for Discord and Telegram simultaneously |
| `email` | Email monitoring with LLM-based filtering (forwards human emails to Telegram) |
| `health` | Show health status of all services, config, and directories |
| `migrate-db` | Run database migrations |
| `mirror` | Copy a Discord guild into one of yours: the channels, the messages, and the answers back |
| `rolodex` | Per-person records from Discord, Telegram, GitHub, LinkedIn and Skool |
| `telegram-channel-watch` | Telegram channel watching (poll/info forwarding) |
| `twitter` | Twitter operations |
| `twitter-schedule` | Twitter scheduled posting |
| `youtube` | YouTube operations |

All commands other than `health`, `migrate-db` and `rolodex` run as daemons.

### `mirror`

`mirror` writes the channels of one guild into another guild. It gives each new channel the name of
the source channel, with `_` in front. It then sends each new message from the source to the copy,
and each new message in the copy back to the source.

Set `[mirror] source_guild` and `[mirror] target_guild` in the config. `mirror` uses the Discord
token at `[dms.discord]`.

| Command | Description |
|---------|-------------|
| `mirror --dry-run` | Print the channels that `mirror` makes. Make no channel. |
| `mirror` | Make the channels, then send the new messages both ways. |

`mirror` asks you if it must also copy the full history. Answer `y` to start. The copy is slow,
because Discord limits how fast a program can write. If you stop it, the next start continues from
the same place. A daemon has no keyboard, so it always answers `n`.

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

### Select members with SQL

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
