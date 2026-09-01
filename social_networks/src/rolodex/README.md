# rolodex

A local directory of per-person Nix files, one per person, fed from the platforms we already hold
sessions for. The file is the single source of truth about a person; nothing it holds is ever
written back to a platform — `dm` sends only what you type on the command line.

```
                  ┌──────────────────────────────┐        ┌─ extract() ────────► log + summary ─┐
   rolodex pull ──┤ fetch ─► diff vs cursor       ├─► Delta┤                                     ▼
                  └──────────────┬───────────────┘    ▲   └─ discover_handles() ─► handles ─► <person>.nix
   a live DM (unwired) ──────────┼─────────────────---┘
                                 └──► history::record ──► <person>/<year>.md
```

`Delta` is only constructible when something new surfaced, so the no-op case is the absence of a
value rather than a guarded call, and `extract` stays ignorant of what surfaced the information.

Two calls rather than one: extraction is told to write down everything still true, discovery that it
will almost always find nothing, and one prompt cannot carry both. `discover_handles` reads a handle
the person stated outright out of their own messages, and is skipped entirely when their `handles`
already cover every fetchable platform. What it finds is fetched by the *next* pull — the same
two-pull cadence discord's connected accounts already run on. A wrong handle needs no verification
step: its first fetch fails, which is reported per handle and leaves the rest of the pull alone.

```
[rolodex] path ──► <dir>/<person>.nix  ◄── human edits        <dir>/<person>/
                        │      ▲                                2019.md … 2026.md   the conversation
               nix eval │      │ render (full regen: comments     assets/*.avif      its images
                        ▼      │         and formatting are lost) meta.json          every cursor
                     Person ───┘
```

The transcript is the durable artifact and the labels in `<person>.nix` are derived from it, so
`meta.json` is written *before* the extraction: a failed LLM call costs a re-run, never a message.
`nix eval` filters on `\.nix$`, which is what keeps the person directory invisible to it.

Two states per person, in [`history`](history.rs):

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

`open [pattern]` and `pull [pattern]`. A pattern matches the file stem or any handle, so
`pull dev_ardi` reaches `orion.nix`. No pattern means fzf for `open`, everybody for `pull`.

`dm <--discord|--telegram|--twitter> <pattern> <text>` takes the same pattern but refuses anything
other than exactly one match: a wasted fetch is recoverable, a message to the wrong person is not.
The flag names the `handles` key it sends through, so a person without that handle is an error
rather than a guess. Discord and telegram reuse the sessions `pull` reads with; twitter sends from
the `[twitter.oauth]` account.

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

Skool contributes a bio, a location and the profile's outbound links. It is the one source that
never needs credentials: a `[skool]` section only adds the posts of groups it shares with them, and
their absence reads as no activity rather than as a failure.

`pull` uses its own telegram session file, seeded from the `dms` daemon's on first use: same
authorization, no write contention with the daemon.
