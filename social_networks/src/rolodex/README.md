# rolodex

A local directory of per-person Nix files, one per person, fed from the platforms we already hold
sessions for. The file is the single source of truth about a person; our side is never written back
to a platform.

```
                  ┌──────────────────────────────┐
   rolodex pull ──┤ fetch ─► diff vs checkpoint   ├─► Delta ─► extract() ─► log + summary
                  └──────────────────────────────┘    ▲                          │
   a live DM ─────────────────────────────────────────┘                          ▼
                                                                          <person>.nix
```

`Delta` is only constructible when something new surfaced, so the no-op case is the absence of a
value rather than a guarded call, and `extract` does not know what surfaced the information.

```
[rolodex] path ──► <dir>/<person>.nix  ◄── human edits
                        │      ▲
               nix eval │      │ render  (full regen: comments and formatting are lost)
                        ▼      │
                     Person ───┘
                        ▲
        db.sqlite3 rolodex_checkpoints(person, source) → cursor
```

Checkpoints live in sqlite rather than the file, so regenerating a person can never clobber them.

`handles` maps platform → handle. `discord` and `telegram` are the two `pull` fetches; the rest are
seeded from discord's connected accounts and exist for a human to read. A handle that stops
resolving takes only itself down — its checkpoint is left alone and the pull continues.

The telegram session is seeded from the `dms` daemon's own session file: the auth key travels with
the file, so the copy is authorized without a login code, while the separate sqlite file keeps this
off the daemon's write path.
