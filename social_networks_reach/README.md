# reach

The transcript format of the on-demand axis, and the store that holds it. Everything about *how* to
talk to a platform is one layer down, in `social_networks_adapters::reach`; this crate knows only
what an [`Item`](social_networks_adapters::reach::Item) is worth keeping as.

```text
 adapters::reach   Profiles · Direct · Venue          the waist
        ▲
 this crate        history   <person>/<year>.md       DMs, cursors, the backfill's two states
                   venue     venues/<p>/<slug>/…      transcript, roster, cursor
                   recon     the venue axis, hand-run
        ▲
 social_networks   people, labels, extraction
```

One line format for both, because both are the same thing seen from a different side:

```markdown
## 2026-03-04

- 14:03:40 [orion/discord] yeah, v1 is out          a person's file: the slot is them
- 14:03:40 [lory/skool@20kmodrop] shipped it        a venue's: the slot is the author and the place
```

The prefix is fixed because it is what a `pull` matches a person's own venue lines on — `[<handle>/`
plus the indented lines under it. That is the whole selection rule; there is no index, and an index
would be rebuilt from this anyway.

A venue transcript keeps the whole conversation, including people nobody tracks: a thread with the
non-members cut out is not the thread. Nothing of it is copied into a person's file, which stays
their DMs.

`recon` is the CLI over it, and is never invoked by a daemon — rate-limit and account-safety exposure
stays human-initiated. It is a binary of this crate rather than a subcommand of `social_networks` so
that the app cannot wire it by accident.
