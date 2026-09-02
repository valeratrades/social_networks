---
name: outreach
description: "Run a cold-outreach campaign over the rolodex: pick who has never been written to, draft one message each off a base message the user supplies, and send them one at a time on confirmation. Triggers on \"we'll be doing outreach\", \"draft messages for\", \"who should I write to in <venue>\", \"campaign\", or a base message handed over with a group named."
---

# outreach

A campaign is: a set of people nobody has talked to, one base message the user wrote, and one file
per person that is that base message plus at most one appended paragraph. The value is in the
restraint. Almost every draft should come out identical to the base.

Runs on top of `/rolodex`, which owns the reading and the sending. This skill owns the selection,
the drafting and the discipline.

## The one invariant

**Only people with no message history.** Never draft to somebody a conversation is already on
record with — a cold opener sent into a live thread is the one unrecoverable mistake here.

Everything else about the selection is the user's to specify and is different every campaign: a
venue, a region, a predicate over the roster, an explicit list of names. Ask for it if the request
does not carry it; do not invent a filter.

## Before drafting: do you actually understand the business?

Read the base message first. It is the only statement of what this campaign is for. From it, name
out loud:

- the industry and the specific model being run
- where **we** stand in it — starting, operating, scaling, which niche is settled on
- what therefore makes another person's history valuable to us

If you cannot fill all three with specifics — not paraphrase of the base message, actual mechanics
of the trade — **stop and ask**. Ask for a link to something we already have rather than a written
explanation: at the point somebody is running an outreach campaign in a niche, the material almost
always already exists. Likely places, worth checking before asking:

- the venue's own transcripts under `<rolodex>/venues/<platform>/<slug>/*.md` — the group is
  usually a course, and the members argue the mechanics in the open
- an earlier `tmp/outreach/` or campaign directory
- `docs/` in whatever repo the campaign is being run out of
- the person files of the people we *have* talked to

This gate is not optional politeness. Without it you cannot tell a person who solved our hardest
problem from a person repeating a platitude, and the whole campaign degrades to sending the base
message to everybody. Getting it wrong is worse than asking.

The niche and the reason for writing are **never** hardcoded here. The style below is.

## Pipeline

### 1. Selection

```
nix develop -c cargo r -p social_networks -- rolodex cold [pattern]
```

`cold` is the list. Apply the user's extra filter on top of it, reading `__main__.nix` and, for
location, the venue's `members.json` (`lat`/`lon`/`zone` — coarse on purpose).

Two failure modes to check for, once per campaign:

- **A name the user assumes is on the list may have no record at all.** `cold` cannot list somebody
  who has no directory under `people/`. If the user names a person, confirm they exist before
  reporting on them; if not, make the skeleton and `rolodex pull` them, then say plainly that they
  were absent rather than cold.
- **`meta.json` can lie.** It records what a read *returned*, so a broken read path writes
  `"messages": 0` and a campaign then treats everybody on that platform as never-contacted. Confirm
  the cold list once against the platform's own state — for skool, the open chat channels — rather
  than trusting the local record. This was a real bug; do not assume it is the last one.

Exclude anybody the user says they have already written to, even if `cold` still lists them.

### 2. Read them

```
nix develop -c cargo r -p social_networks -- rolodex lines <pattern>
```

Their own words out of the venue transcripts. **This is the only real source of personalisation.**
A profile bio is not. Someone whose entire footprint is `skool:bio = "Web design agency"` gets the
base message and nothing else.

Read every selected person's lines before drafting any of them — the judgement is comparative.
Someone with 20 lines who runs the thing we are trying to run is a different message from someone
with 2 lines asking a beginner question.

### 3. Draft

Layout, under `tmp/outreach/`:

```
tmp/outreach/
  base_msg.md      the user's message, verbatim, persisted before anything else
  <stem>.md        one per person; <stem> is exactly their directory name under people/,
                   accents and all (istván-hag.md, not istvan-hag.md)
```

**Demand the base message as a file.** If the user pastes it inline, write it to `base_msg.md`
first and work off the file. Every draft is a copy of it plus at most one appended paragraph. When
in doubt, `cp base_msg.md <stem>.md` and move on.

### 4. Report, then send

Report which drafts deviate from base and what each appended line is — one line each, no prose.
Then stop. **Nothing is sent without the user saying to send.**

```
nix develop -c cargo r -p social_networks -- rolodex dm --<platform> <stem> "$(cat tmp/outreach/<stem>.md)"
```

One person per invocation. `dm` refuses a pattern matching anything but exactly one person; that
refusal is a safety property and is never worked around by broadening the pattern or looping over
matches. **Delete each draft once it is sent**, so the directory is always the queue of what is
still outstanding.

"Send the first N" means the first N in directory order unless the user says otherwise.

## The base message is law

The user wrote it. It is not a first draft for you to improve.

- **Do not touch its spacing, capitalisation, punctuation or grammar.** If it starts sentences
  lowercase, keep them lowercase. If it reads slightly clumsy ("proportional part of future
  profits"), that is a voice, not an error. Never fix it.
- **Do not weave personalisation into its sentences.** Additions go **below**, as a new paragraph.
- Small global substitutions the user asks for (a country widened to a continent, a newly-decided
  detail inserted) are applied to `base_msg.md` itself so every draft inherits them.

There is exactly **one** permitted mutation of the base per person: **drop a conditional question
the evidence already answers.** If the base asks "you making money from this?" and their transcript
shows they plainly are — or plainly are not — the question reads as not having listened. Drop the
question and the `If yes,` that hangs off it; keep the rest of the sentence intact. Nothing else
about the base ever changes per person.

## The appended paragraph

At most one, prefixed `btw, `, separated by a blank line. It must be **a question about something
they themselves wrote**. That is the whole permitted space.

Two shapes work:

- **Did you solve the problem you posted about?** — they asked the group something, or reported
  something broken. `btw, did you get the plumbing GMB verification sorted?`
- **A specific question their demonstrated expertise answers.** Narrow enough that they can reply in
  one line. `btw, is 700 monthly searches still the floor you'd use in the UK?`

If you cannot derive either from their own words, **append nothing**. That is the correct outcome
for most people.

If somebody is plainly a heavy operator and you still cannot derive a specific ask, put a literal
`TODO:` line in their file and surface it in the report with the raw quotes the user needs to write
it themselves. Do **not** invent an ask to fill the space.

But do not reach for `TODO:` because your ask feels too small for them. A cold opener gets one
question from a busy person regardless. One narrow question that is easy to answer beats a grand
one that is not. `TODO:` is for "I cannot derive an ask", never for "my ask undersells them".

## Style

Hardcoded, campaign-independent, and mostly a list of things not to do. The failure mode is
uniform: text that reads as written by an AI trying to demonstrate that it read carefully.

**Never:**

- **Rhetorical contrast pairs.** `suspension as a structural thing and not an accident`,
  `somebody who'd actually done it rather than theorised about it`, `everyone else is buying them
  and watching them fall off`. This is the single most frequent tell. If a sentence sets up an X-not-Y
  or X-while-others-Y shape, delete it.
- **Ranking them against the group.** `the only one in that group who…`, `the sharpest thing anybody
  there has said`, `further down that path than most`. Flattery that also proves you surveilled
  everybody.
- **Explaining why you are asking.** `I'm at the same step`, `that's the part I'd rather learn than
  rediscover`, `that's the mistake I'm trying not to buy myself`. Ask the question and stop. If the
  reason mattered they would ask.
- **Pitches, offers, or anything committing the user to a future action.** No buying a service, no
  hiring, no offering to build something, no proposing to work together. `do you take trades in
  europe? that's exactly what I'll need booked` and `I can just write that script for you, it's an
  hour of work` are both out — the user never said either, and now they are on the hook for it.
  Only the user pitches. You only ask about what they already said.
- **Em dashes.** The voice uses `, - ` (comma, space, hyphen, space). Match whatever the base does.
- **Quoting them back at length.** One clause of reference is plenty; a verbatim block reads like
  surveillance.

**Do:**

- Get to the point in the first clause.
- Keep it to one or two sentences. If it needs a comma-heavy build-up, it is wrong.
- Match the base's register exactly — lowercase openings, contractions, the lot.
- Prefer the plainest possible phrasing. Nothing here is trying to impress anybody.

The test: if pitches and flourishes were permitted, would the user have written it this tersely? If
your line is longer than what they would have typed, cut it.

## Judgement calibration

From one real campaign of 26 people: **20 got the base message verbatim.** Six got an appended
question. Two of those needed a `TODO:` instead of an invented ask.

If more than a quarter of your drafts deviate from base, you are personalising off bios and
platitudes. Go back and cut.
