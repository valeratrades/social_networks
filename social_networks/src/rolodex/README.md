# rolodex

A local directory of per-person Nix files, one per person, fed from the platforms we already hold
sessions for. The file is the single source of truth about a person; nothing it holds is ever
written back to a platform — `dm` sends only what you type on the command line.

```
                  ┌──────────────────────────────┐        ┌─ extract() ────────► log + summary ─┐
   rolodex pull ──┤ fetch ─► diff vs checkpoint   ├─► Delta┤                                     ▼
                  └──────────────────────────────┘    ▲   └─ discover_handles() ─► handles ─► <person>.nix
   a live DM (unwired) ────────────────────────────---┘
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
[rolodex] path ──► <dir>/<person>.nix  ◄── human edits
                        │      ▲
               nix eval │      │ render  (full regen: comments and formatting are lost)
                        ▼      │
                     Person ───┘
                        ▲
        db.sqlite3 rolodex_checkpoints(person, source) → cursor
```

Checkpoints live in sqlite rather than the file, so regenerating a person cannot clobber them.

`open [pattern]` and `pull [pattern]`. A pattern matches the file stem or any handle, so
`pull dev_ardi` reaches `orion.nix`. No pattern means fzf for `open`, everybody for `pull`.

`dm <--discord|--telegram|--twitter> <pattern> <text>` takes the same pattern but refuses anything
other than exactly one match: a wasted fetch is recoverable, a message to the wrong person is not.
The flag names the `handles` key it sends through, so a person without that handle is an error
rather than a guess. Discord and telegram reuse the sessions `pull` reads with; twitter sends from
the `[twitter.oauth]` account.

`handles` maps platform → handle. `discord`, `telegram` and `github` are what `pull` fetches; the
rest are seeded from discord's connected accounts and exist for a human to read. A handle that stops
resolving takes only itself down — its checkpoint is left alone and the pull continues.

Github contributes a bio and a public event feed. The feed is filtered to the event types that can
carry signal before it reaches the prompt, which then holds it to a much higher bar than DMs.

`pull` uses its own telegram session file, seeded from the `dms` daemon's on first use: same
authorization, no write contention with the daemon.
