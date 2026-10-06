Fill in `~/.config/social_networks.nix`. Follow [examples/config.nix](../../examples/config.nix).

## Commands

| Command | Description |
|---------|-------------|
| `dms` | DM monitoring (ping, monitored users) on Discord, Telegram and Skool. `[dms] sources` selects which of them. |
| `email` | Email monitoring with LLM-based filtering (forwards human emails to Telegram) |
| `health` | Show health of config and directories (daemon liveness is in devops Grafana) |
| `migrate-db` | Run database migrations |
| `purpose <name>` | Per-person records for one purpose, from Discord, Telegram, GitHub, LinkedIn and Skool |
| `rolodex` | The same as `purpose rolodex` |
| `telegram-channel-watch` | Telegram channel watching (poll/info forwarding), on its own session |
| `twitter` | Twitter operations |
| `twitter-schedule` | Twitter scheduled posting |
| `youtube` | YouTube operations |

All commands other than `health`, `migrate-db`, `purpose` and `rolodex` run as daemons.

### `purpose`

A purpose is one use of the person files: your own connections (`rolodex`), leads to ask for
reviews (`reviews`), and so on. Each purpose has a folder, a list of tags with a type each, the ways
to add people (`procure`), and a ranking (`rank`). The config names them under `purposes`, and
`venues` is the folder that `recon` writes groups to. All purposes use the same groups.
[examples/purposes](../../examples/purposes) has two.

| Command | Description |
|---------|-------------|
| `rolodex open [pattern]` | Open a person file in `$EDITOR`. Create the file if the pattern finds nobody. |
| `rolodex pull [pattern]` | Get new data for each person the pattern finds. Write it to their files. |
| `rolodex procure [<strategy> \| <platform>:<slug>]` | Make a file for each person a strategy selects that has no file yet, and put its tags on each person it selects. |
| `rolodex rank [pattern]` | Show each person in order of the ranking, with the part each term gives. |
| `rolodex cold [pattern]` | The same as `rank`, for each person that you sent no message to and got no message from. |
| `rolodex tag [<name>[=<value>]] [pattern]` | Put a tag on each person the pattern finds. Without a name, show the tags. |
| `rolodex lines [pattern]` | Show what each person wrote in the groups. |
| `rolodex prune` | Remove each person that left every group and holds no conversation. |
| `rolodex dm <--platform> <pattern> <text> [--noise <min>..<max>]` | Send one message to one person. |
| `rolodex send -n <n> [pattern] [--noise <min>..<max>] [--chance-of-distraction <p>] [--distraction-duration <s>]` | Send what is due in the `outbox/` of exactly `n` people, best ranked first. |

For another purpose, write `purpose <name>` in place of `rolodex`.

A pattern finds a person by file name or by any handle. Without a pattern, `open` starts `fzf` and
`pull` takes everybody.

`pull` also keeps the messages. It writes them to `<person>/<year>.md`, next to the person file. The
first `pull` gets the full history of each conversation, and can take a long time. If you stop it,
the next `pull` continues from the same place.

`--noise 15..60` browses facebook for 15 to 60 seconds after each message, in the same chrome: the
feed, a profile, a search. Facebook only.
Before each facebook message, `send` browses like that with a chance of `--chance-of-distraction`
(0.2), for half to one and a half of `--distraction-duration` (30 s).

`cold` finds each person that holds no conversation with you. A line that a person wrote in a group
is not a conversation, so each member that `procure` added stays cold. `cold` checks every platform
that you keep a handle for. It uses the messages that `pull` kept. If `pull` read no messages from a
platform, `cold` asks that platform for one message. It keeps no message that it gets.

`rank` gives each person a score from 0 to 100. Each term of the ranking reads one tag, or one value
from the messages: `interactions` (the days they wrote to you), `last_interaction` (the last message),
or `venue_activity` (their lines in the groups). A term gives 0 if the person has no value for it. A
`decay` on a term sets how much it decreases the weight of an old line. With `decay = 0`, each line
has the same weight.

```
score = Σwv/Σw  ×  (1 − 2^(−Δt / unanswered_half_life))    only while their newest line is ours
         └ terms ┘   └──── on the whole result ────┘
         just sent ×0 · 1w ×½ · 2w ×¾ · 4w ×15/16 · they reply → ×1
```

So a person you wrote to drops out of the top, and comes back as the decay wears off or as soon as
they answer. `stale_half_life` sets how fast what a `pull` refreshes goes out of date: `stale` in
`rank` is how far a `pull` could move the score, and `pull --top <n>` takes the `n` stalest.

Each `pull` reads the groups a person is in from their profile and writes them to their file. `cold`
removes each person that is in no group you keep, and shows their names. `prune` deletes those files
if no conversation is in them. The group files keep the lines of these people.

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
| `recon find skool:<slug> <term>` | Search the members of the group for a term. Skool only. |

The group files go under `<venues>/<platform>/<slug>/`. `rolodex pull` then reads the
lines of each person you keep a file for, and `rolodex lines` shows them to you. `recon` gets the
posts one time, and every read after that is free.

`recon posts` gets the posts and the replies to them. Without `--since`, it gets what is new since
the last read. With `--since`, it goes back to that day and gets everything after it. The store adds
to its files and does not rewrite them, so to build the group again from the start, delete its
`<year>.md` and `meta.json` first and keep `members.json`.

`recon find` asks the group the same question its own search bar asks. The term matches the start of
a word in a handle or in a name. The group answers 10 members and no more, and it gives no cursor.
So use `find` to get one person, and `members` to get the list. `find` reads members that
`members` cannot: a member with no map pin is not on the roster, and `find` still gets them.

### Select members with SQL

`--where` takes a SQL `WHERE` clause. It also takes a path to a file that holds one. `recon roster`
and `rolodex procure` use the same clause and the same table.

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
rolodex procure skool:<group> --where ~/rolodex/queries/europe.sql --dry-run
rolodex procure skool:<group> --where ~/rolodex/queries/europe.sql
rolodex pull <stem>
```

`procure` writes a file for each selected person who has no file. `pull` then fills each file.

Two limits apply to skool, and both make the member list shorter than the group:

- Skool gives a position only for the members who gave one. In a group of 406, 325 gave one.
- Skool does not page its member list. The page parameter changes the page number in the payload,
  but the payload always holds the first 30 members.

`recon members` uses the group map and the member page together. It reads one page for the map and
one request for each member on it, so it is slow and it waits between requests. Do it one time for
each group. The `zone` column is a second signal, but the person sets it in the browser, so it can
disagree with the position.
