# rolodex

A local directory of per-person directories, fed from the platforms we already hold sessions for.
`__main__.nix` is the single source of truth about a person; nothing it holds is ever written back to
a platform — `dm` sends only what you type on the command line.

```
                  ┌──────────────────────────────┐        ┌─ extract() ────────► log + summary ─┐
   rolodex pull ──┤ fetch ─► diff vs cursor       ├─► Delta┤                                     ▼
                  └──────────────┬───────────────┘    ▲   └─ discover_handles() ─► handles ─► <person>/__main__.nix
   a live DM (unwired) ──────────┼─────────────────---┤
   venue lines by this person ───┼──────────────────--┘
                                 └──► history::record ──► <person>/<year>.md
```

Three inputs, and only two of them cost a request. The third is the venue transcripts `recon` already
wrote: every line whose slot reads `[<handle>/` for a handle of theirs, since the last one the
extraction saw. They join as `Kind::Post` items and go nowhere near the person's own year files —
what they said in a group belongs to the group's transcript, not to their DMs.

`Delta` is only constructible when something new surfaced, so the no-op case is the absence of a
value rather than a guarded call, and `extract` stays ignorant of what surfaced the information.

Two calls rather than one: extraction is told to write down everything still true, discovery that it
will almost always find nothing, and one prompt cannot carry both. `discover_handles` reads a handle
the person stated outright out of their own messages, and is skipped entirely when their `handles`
already cover every fetchable platform. What it finds is fetched by the *next* pull — the same
two-pull cadence discord's connected accounts already run on. A wrong handle needs no verification
step: its first fetch fails, which is reported per handle and leaves the rest of the pull alone.

```
                                    ┌─ __main__.nix ──► Person   what we say about them
                                    │        ▲   ▲
[rolodex] path ──► <dir>/<person>/ ─┤        │   └── human edits
                                    │        └── render (full regen: comments and
                                    │                   hand formatting are lost)
                                    ├─ 2019.md … 2026.md   the conversation
                                    ├─ assets/*.avif       its images
                                    └─ meta.json           every cursor
```

The transcript is the durable artifact and the labels in `__main__.nix` are derived from it, so
`meta.json` is written *before* the extraction: a failed LLM call costs a re-run, never a message.
Holding a `__main__.nix` is what makes a directory a person's, so `venues/` and anything else living
under the same root need no naming.

Two states per person, in [`history`](../../../social_networks_reach/src/history.rs):

```
 BACKFILLING                                    STEADY
 every message → jsonl under $XDG_CACHE_HOME    every new message → append <year>.md
 no year files yet                              the cache is gone
 meta saved after every page                    append-only: no parser, no rewrite
      └──── all sources backfill_done ────► render the year files once, drop the cache ────┘
```

A backfill walks backwards and runs to the first message of the conversation, over as many pulls as
it takes. Holding every year file back until the last source is done is what makes each of them a
single whole-file write, and removes the seam between two sources that reached different depths.
`github` and `linkedin` carry no message history, so they are born `backfill_done`.

A year file, times in UTC, continuation lines indented two spaces so the list item stays open:

```markdown
## 2026-03-04

- 14:03:40 [orion/discord] yeah, v1 is out
  ![](assets/discord-1349938102838738944.avif)
- 14:05:02 [orion/discord] [adapter_bench.csv]
```

Images are converted to avif once under a name their own id determines, so a re-download is free and
an orphan from a failed pull is harmless. Everything else is named and not kept.

`open [pattern]` and `pull [pattern]`. A pattern matches the directory name or any handle, so
`pull dev_ardi` reaches `orion/`. No pattern means fzf for `open`, everybody for `pull`.

`discover <platform>:<slug>` is the other axis arriving: it reads the roster and transcript `recon`
wrote and leaves a skeleton file for everyone the selection names and nobody has yet. `pull` needs
nothing more than a handle, so a skeleton is the whole handover.

```
rolodex discover skool:20kmodrop --active-since 90d --min-posts 2 --dry-run
rolodex discover skool:20kmodrop --where 'posts > 5 AND joined > "2026-01-01"'
```

The query language is SQL because the selection *is* relational — a roster joined against its own
line counts — and any grammar of our own would converge on SQL, worse. `libsql` was already a
dependency; `select` builds a few hundred rows in memory, runs the `WHERE`, and keeps nothing. The
flags desugar into that same clause, so there is one evaluator. `--where` takes inline SQL or a path
to a `.sql` file, told apart by asking the filesystem. Columns: `handle`, `display`, `joined`,
`posts`, `first_post`, `last_post`.

Directory names are `<first>-<last>` off the display name, the handle when there is nothing else,
and a numeric suffix on collision. `discover` prints what it wrote so one can be `git mv`'d — the
name is not load-bearing, since a pattern searches handles too.

`cold [pattern]` is the other end of that handover: everybody no conversation is on record with, on
any platform that could hold one. A venue line is not one — it never entered their year files — so a
member `discover` wrote a file for stays cold until they are written to.

Every attached source is checked. `meta.json` answers for whatever a pull has already kept, and a
source it says nothing about is asked outright, for a single message: the question is whether
anything is there, not what it says. Nothing is written — the messages are `pull`'s, and a probe
that checked one in would leave a transcript no backfill may finish. A source that errors excludes
the person rather than listing them, since a request that did not complete is not a "no".

`dm <--discord|--skool|--telegram|--twitter> <pattern> <text>` takes the same pattern but refuses anything
other than exactly one match: a wasted fetch is recoverable, a message to the wrong person is not.
The flag names the `handles` key it sends through, so a person without that handle is an error
rather than a guess. Every one of them goes out through the same `Direct::send` the reads come in
through: discord and telegram over the sessions `pull` uses, twitter from the `[twitter.oauth]`
account, skool over a chat channel it opens through a shared group.

`handles` maps platform → handle. `discord`, `telegram`, `github`, `linkedin` and `skool` are what
`pull` fetches; the rest are seeded from discord's connected accounts and skool's profile links, and
exist for a human to read. A handle that stops resolving takes only itself down — whatever its
backfill already checked in stands, and the pull continues.

Github contributes a bio and a public event feed. The feed is filtered to the event types that can
carry signal before it reaches the prompt, which then holds it to a much higher bar than DMs.

Linkedin is read logged out, under no credentials, for the headline and about text — where someone
works now, which no other source states. Anonymous views are authwalled after a handful, so its
cursor is the date of the last success and a profile fetched within 30 days is skipped: the wall
turns into a queue that drains over successive pulls instead of a failure to design around.

Skool contributes a bio, a location, a display name and the profile's outbound links. It is the one
source that never needs credentials: a `[skool]` section only adds the posts of groups it shares with
them, and their absence reads as no activity rather than as a failure. Its groups are a different
matter — those are `recon`'s, and they need both credentials and a membership.

`pull` uses its own telegram session file, seeded from the `dms` daemon's on first use: same
authorization, no write contention with the daemon.
