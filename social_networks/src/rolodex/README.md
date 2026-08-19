# rolodex

A local directory of per-person Nix files, one per person, fed from the platforms we already hold
sessions for. The file is the single source of truth about a person; our side is never written back
to a platform.

```
                  ┌──────────────────────────────┐
   rolodex pull ──┤ fetch ─► diff vs checkpoint   ├─► Delta ─► extract() ─► log + summary
                  └──────────────────────────────┘    ▲                          │
   a live DM (unwired) ────────────────────────────---┘                          ▼
                                                                          <person>.nix
```

`Delta` is only constructible when something new surfaced, so the no-op case is the absence of a
value rather than a guarded call, and `extract` stays ignorant of what surfaced the information.

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

`open [pattern]` and `pull [pattern]`. A pattern matches the file stem, `name`, or any handle, so
`pull dev_ardi` reaches `orion.nix`. No pattern means fzf for `open`, everybody for `pull`.

`name` is free text `pull` never writes: whatever a person should be findable by beyond their stem
and handles. Everything else in the file is regenerated.

`handles` maps platform → handle. `discord`, `telegram` and `github` are what `pull` fetches; the
rest are seeded from discord's connected accounts and exist for a human to read. A handle that stops
resolving takes only itself down — its checkpoint is left alone and the pull continues.

Github contributes a bio and a public event feed. The feed is filtered to the event types that can
carry signal before it reaches the prompt, which then holds it to a much higher bar than DMs. Reads
authenticate with `gh auth token`; without it github allows 60 requests an hour for the whole host.

`pull` uses its own telegram session file, seeded from the `dms` daemon's on first use: same
authorization, no write contention with the daemon.
