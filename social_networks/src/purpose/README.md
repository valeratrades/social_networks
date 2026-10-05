# purpose

A local directory of per-person directories, fed from the platforms we already hold sessions for.
`__main__.nix` is the single source of truth about a person; nothing it holds is ever written back to
a platform — `dm` sends only what you type on the command line.

How somebody was found must not dictate how they are dealt with, so the store, the transcripts, the
tags, the ranking and the outreach are one machinery, and a **purpose** says only what differs: its
folder, how people are procured into it, its tag vocabulary, and its ranking. `rolodex <cmd>` is
`purpose rolodex <cmd>`.

```
config.nix
├─ venues = "/…/venues"                 shared: a venue feeds any purpose (`recon` writes here)
└─ purposes.<name>
     ├─ path     folder whose children are person dirs
     ├─ tags     { <name> = { type = bool | text | number{min;max} | birthday | place | timestamp; about?; };
     │             <group> = [ "<value>" … ]; }
     ├─ procure  { <name> = { venue = "skool:x"; where = "<sql, may name $<group>>"; tags = { <group> = "$<group>"; … }; }; }
     ├─ rank     [ { of = <tag | builtin>; weight; <shape params> } … ]
     └─ half_life how fast what a pull refreshes goes out of date, e.g. "60d"
```

The whole purpose is checked at load, and fails by name: a rank term whose shape does not fit what
it reads, a tag named like a builtin, an `about` on a type extraction cannot fill, a strategy tag of
the wrong type, a `$<name>` that names no group.

```
                  ┌──────────────────────────────┐        ┌─ extract() ───► log, summary, tags ─┐
   pull ──────────┤ fetch ─► diff vs cursor       ├─► Delta┤                                     ▼
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
<purpose> path ─── <person>/ ──────────┤        │   └── human edits
                                       │        └── render (full regen: comments and
                                       │                   hand formatting are lost)
                                       ├─ 2019.md … 2026.md   the conversation
                                       ├─ assets/*.avif       its images
                                       └─ meta.json           every cursor

venues ─────────── <platform>/<slug>/   `recon`'s axis, read at pull time
```

The transcript is the durable artifact and the labels in `__main__.nix` are derived from it, so
`meta.json` is written *before* the extraction: a failed LLM call costs a re-run, never a message.
Holding a `__main__.nix` is what makes a directory a person's, so a stray directory in a purpose's
folder costs nothing.

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

`open [pattern]` and `pull [pattern] [--top n]`. A pattern matches the directory name or any handle, so
`pull dev_ardi` reaches `orion/`. No pattern means fzf for `open`, everybody for `pull`. `pull` walks
them stalest first, and `--top n` stops after the n stalest.

`procure` is the other axis arriving: it reads the roster and transcript `recon` wrote, leaves a
skeleton for everyone a strategy selects and the purpose lacks, and puts the strategy's `tags` on
everyone it selects. Nobody is re-created. `pull` needs nothing more than a handle, so a skeleton is
the whole handover, and nothing here fetches — `recon` stays the only thing that spends a request.

```
rolodex procure                                  # every strategy in `procure`
rolodex procure servicing                        # one of them
rolodex procure skool:20kmodrop --active-since 90d --min-posts 2 --dry-run
rolodex procure skool:20kmodrop --where 'posts > 5 AND joined > "2026-01-01"'
rolodex procure servicing --location london      # a strategy generic over the `location` group
```

A strategy is generic over every group its `where` or a tag value names as `$<group>`, and runs only
once each is bound by `--<group> <value>`. The flags exist per purpose, so `procure` takes its
arguments raw and parses them against a command built from the purpose's groups — `--help` lists
the values. One `--location` binds every strategy run that is generic over it; one that is not
ignores it, and a flag nothing run is generic over is refused. A value is a bare word, so it is
spliced into the SQL as is.

The query language is SQL because the selection *is* relational — a roster joined against its own
line counts — and any grammar of our own would converge on SQL, worse. `libsql` was already a
dependency; `select` builds a few hundred rows in memory, runs the `WHERE`, and keeps nothing. A
strategy's `where`, the flags and `--where` are ANDed into that same clause, so there is one
evaluator. A clause is inline SQL or a path to a `.sql` file, told apart by asking the filesystem.
Columns: `handle`, `display`, `joined`, `lat`, `lon`, `zone`, `place`, `bio`, `posts`, `first_post`, `last_post`.

Directory names are `<first>-<last>` off the display name, the handle when there is nothing else,
and a numeric suffix on collision. `procure` prints what it wrote so one can be `git mv`'d — the
name is not load-bearing, since a pattern searches handles too.

`rank [pattern]` is who to reach first: everybody matching, in order, with each term's share.

```
                   ┌── tags (typed, checked against the vocabulary at load)
person dir ────────┤
                   └── year files ──► builtins: interactions, last_interaction, venue_activity
                                  │
   rank term: value ∈ [0,1] (absent → 0, shown as ·) ──► score = Σ w·v / Σ w
```

| type / builtin | term params | value |
|---|---|---|
| bool | — | 1 / 0 |
| number | — | linear within its declared bounds |
| birthday | `within = [lo hi]` ages | fraction of the ages it allows today inside the target |
| place | `near = {lat; lon; radius_km; halving_km}` | 1 inside the radius, `0.5^((d−r)/halving)` beyond it |
| timestamp, `last_interaction` | `decay` | recency over the cohort |
| `interactions` | — | ÷ cohort max |
| `venue_activity` | `decay` | activity over the cohort ÷ cohort max |

Absent means no credit, so every term is a bonus: a penalty for distance is a lost closeness bonus,
which orders the same. The builtins are derived from the transcripts at rank time and never stored —
`interactions` is the distinct days with a line by them in their year files, `last_interaction` their
newest year-file line in either direction, `venue_activity` their lines across every venue
transcript. Recency is `ln(age)` over the cohort, as [`rank`](../../../social_networks_reach/src/rank.rs)
explains, so a score means nothing outside the cohort it was ranked in. A person still backfilling has
no year files yet, and is marked rather than read as having none.

`stale` is what a pull stands to move somebody's score by, in score points:

```
stale = Σ_t  (w_t / Σw)  ·  (1 − 2^(−Δt / half_life))  ·  E|v_t − V_t|
             └ share ┘      └ P(changed since synced) ┘    └ how far it would move ┘

Δt   since what refreshes t:  a fact, interactions, last_interaction → the last pull every handle answered
                              a tag with an `about`                 → the last time a model read all of it
                              anything else (timestamps, venue_activity, a tag without `about`) → nothing: 0
     never ⇒ the factor is 1
V_t  t's values across those already synced, plus one uniform draw on [0,1] — so an empty cohort still spreads
```

`cold [pattern]` is `rank` restricted to everybody no conversation is on record with, on any platform
that could hold one. A venue line is not one — it never entered their year files — so a member
`procure` wrote a file for stays cold until they are written to.

Every attached source is checked. `meta.json` answers for whatever a pull has already kept, and a
source it says nothing about is asked outright, for a single message: the question is whether
anything is there, not what it says. Nothing is written — the messages are `pull`'s, and a probe
that checked one in would leave a transcript no backfill may finish. A source that errors excludes
the person rather than listing them, since a request that did not complete is not a "no".

`lines [pattern]` reads the other direction of the same walk `pull` folds into a person's labels:
their own venue lines, whole, with the venue each came from. Nothing is fetched and nothing is
summarised — outreach is written off what somebody actually said.

`dm <--discord|--facebook|--skool|--telegram|--twitter> <pattern> <text>` takes the same pattern but refuses anything
other than exactly one match: a wasted fetch is recoverable, a message to the wrong person is not.
The flag names the `handles` key it sends through, so a person without that handle is an error
rather than a guess. Every one of them goes out through the same `Direct::send` the reads come in
through: discord and telegram over the sessions `pull` uses, twitter from the `[twitter.oauth]`
account, skool over a chat channel it opens through a shared group, facebook typed into Messenger
from `facebook.send`'s chrome, or the burner's without one.

`tags` are the axis no platform has a say in — `venues` and `handles` are what a platform states,
a tag is what is said about them. The vocabulary is the purpose's `tags`, typed, and a tag a person
carries that it does not name, or a value of the wrong type, fails every load by name: a misspelling
would otherwise read as a cohort of one forever. A human writes them (`tag`, `open`), a strategy puts
its own on everyone it procures, and `pull`'s extraction regenerates every tag carrying an `about`,
whole, the same way it regenerates `summary`. Those are the judgements only an LLM can make — how
sharp somebody is, how much they would share — and a purpose pays for exactly the ones it declares.
A judgement that found nothing to support a value is written `null`, so it stays apart from one never
made: a person missing any of them is extracted on the next `pull` whether or not anything new
surfaced, off their year files and venue lines, which is how a tag added to the vocabulary reaches
everybody already in it.

A **fact** is a tag a platform states rather than anybody judging it: `lives_in` (a place) and
`birthday`. A purpose opts in by declaring a tag of that name, and a load refuses it declared as
any other type.

A `birthday` is kept rather than an age, so it never goes stale: the age a `within` term reads is
derived at rank time. It is either a date a platform states (facebook's "September 25, 2002") or a
range of birth years off an age somebody stated — `34` said on a day in 2026 is `1991..=1992`, and a
single year is the range of one. With an `about`, the extraction proposes the newest statement it
sees, with the day it was said. What is there moves only to better evidence: a date over any range
and a later date over an earlier one; a newer statement, or a narrower range inside the old one, over
a range; and an undated statement (a bio, a note) only ever fills a gap. `procure` seeds it off a roster row that places somebody — a facebook City-filter
hit counts as living there, as is, without a visit — and `pull` overwrites it with what a visit
found, removing it when the visit found no current city. So the business location is a `near` rank
term over it, and moving the business costs no visits.

A tag is one name however it is spelled — `ServiceArb`, `service-arb` and `service_arb` are the same
tag in the vocabulary, a person file, a rank term, a `$placeholder`, the command line and a pattern —
and it is kept and shown snake_case. Two spellings of one name side by side fail the load.

A **group** is declared as a list, `location = [ "lyon" "paris" ];`: one value per person out of
it, lowercase `[a-z0-9_-]`, never judged by the extraction, and what a strategy can be generic over.

```nix
tags = { service_arb = true; interest = 0.7; birthday = { min = 1990; max = 1991; as_of = "2026-03-04"; };
         lives_in = { name = "Lyon"; lat = 45.76; lon = 4.84; }; last_login = "2026-09-01T00:00:00Z";
         location = "lyon"; };
```

```
rolodex tag                              # the vocabulary, its types, and how many people carry each
rolodex tag service_arb <pattern>        # a bare name is a bool set to true; --rm takes a tag off
purpose reviews tag birthday=1990 <pattern> # otherwise `<name>=<value>`: 0.7, 1988..1992 or 2002-09-25, Lyon@45.76,4.84, 2026-09-01
rolodex tag location:lyon <pattern>      # a group is `<group>:<value>`
rolodex cold ServiceArb                  # any spelling of a name is that name; a pattern matches a true bool tag whole
rolodex rank location:lyon               # and a `<group>:<value>` whole
```

`handles` maps platform → handle. `discord`, `telegram`, `github`, `linkedin`, `skool` and `facebook`
are what `pull` fetches; the rest are seeded from discord's connected accounts and skool's profile links, and
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

Facebook is visited from the burner's headless chrome, started for the pull only when somebody in it
has a `facebook` handle: the About tab's current city (geocoded into `lives_in`), hometown, birthday,
work and education, and contact links as handles. Every visit is a visit: who is due one is `stale`'s
to say, and somebody visited today sinks to the bottom of the order by themselves.

`pull` uses its own telegram session file, seeded from the `dms` daemon's on first use: same
authorization, no write contention with the daemon.
