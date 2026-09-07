---
name: outreach
description: "Run a cold-outreach campaign over the rolodex: pick who has never been written to, draft one message each off a base message the user supplies, and send them one at a time on confirmation. Triggers on \"we'll be doing outreach\", \"draft messages for\", \"who should I write to in <venue>\", \"campaign\", or a base message handed over with a group named."
---

# outreach

A campaign is: a set of people nobody has talked to, one base message the user wrote, and one file
per person that is that base plus at most one appended paragraph. The value is in the restraint.

Runs on `/rolodex`, which owns the reading and the sending. This skill owns selection, drafting and
discipline.

## The one invariant

**Only people with no message history.** A cold opener sent into a live thread is the one
unrecoverable mistake here. Everything else about the selection is the user's to specify — a venue,
a region, a predicate, a list of names. Ask if the request does not carry it; do not invent a filter.

## Before drafting: do you understand the business?

Read the base message. It is the only statement of what the campaign is for. From it, name the
industry and the model being run, where **we** stand in it, and what therefore makes somebody else's
history worth having. If you cannot fill all three with actual mechanics rather than paraphrase,
**stop and ask** — and ask for a link, not an explanation: the material almost always exists
already, in the venue transcripts under `<rolodex>/venues/<platform>/<slug>/*.md`, in an earlier
`tmp/outreach/`, or in the files of people we have talked to.

Without this you cannot tell somebody who solved our hardest problem from somebody repeating a
platitude, and the campaign degrades to the base message for everybody.

## Pipeline

### 1. Selection

```
nix develop -c cargo r -p social_networks -- rolodex cold [pattern]
```

The list, ranked loudest-first by venue activity and printed as a 0-100 score. `--decay` is how hard
age is discounted on a `ln(age)` axis: `0` counts lines and ignores dates, the default `3` lets one
recent line beat several old ones, past `~10` only the newest survives. Run two settings — a name
that moves a lot is volume-heavy or recency-heavy rather than active.

What the score does not say:

- **Not relevance.** It counts lines, not what is in them. Two lines on our exact niche beat twenty
  on another. Re-sort by relevance yourself, and say which order you used.
- **Not comparable across runs.** Normalised over the cohort listed, so `100` means loudest of these.
- **`·` is not zero** — nothing on disk, the normal state for anybody `discover` added.

Then check, every campaign:

- **Location lies.** `members.json` (`lat`/`lon`/`zone`) is coarse and often wrong: in one 21-person
  cohort it put a self-described USA member in Italy, an `America/Los_Angeles` zone on a Ghent pin,
  and `Europe/Minsk` on a London one. Their own words and `skool:bio` outrank the pin. The roster
  lies about membership too, which is why `cold` reads that off each person's own profile instead
  and prints who it dropped — those people are gone, not deprioritised.
- **`meta.json` lies.** It records what a read *returned*, so a broken read writes `"messages": 0`
  and everybody looks never-contacted. `rolodex pull` over the cold set re-asks; the check is that
  *somebody* comes back non-zero, since a broken read and a genuinely cold cohort look identical.
  Needs live credentials — without them, say the list is unconfirmed rather than verified.
- **A name the user assumes is listed may have no record at all.** `cold` cannot list somebody with
  no directory under `people/`. Make the skeleton, `rolodex pull`, and say they were absent.

Exclude anybody the user says they have already written to, even if `cold` still lists them.

### 2. Read them

```
nix develop -c cargo r -p social_networks -- rolodex lines <pattern>
```

Their own words out of the venue transcripts. **This is the only real source of personalisation.** A
bio is not: somebody whose whole footprint is `skool:bio = "Web design agency"` gets the base and
nothing else. Read every selected person before drafting any of them — the judgement is comparative.

### 3. Draft

```
tmp/outreach/
  base_msg.md      the user's message, verbatim, persisted before anything else
  <stem>.md        one per person; <stem> is their directory name under people/, accents and all
```

**Demand the base as a file.** If the user pastes it inline, write it to `base_msg.md` and work off
the file. When in doubt, `cp base_msg.md <stem>.md` and move on.

### 4. Report, then send

Report which drafts deviate and what each appended line is — one line each, no prose. Then stop.
**Nothing is sent without the user saying to send.**

```
nix develop -c cargo r -p social_networks -- rolodex dm --<platform> <stem> "$(cat tmp/outreach/<stem>.md)"
```

One person per invocation, run from the repo root — a `cd` into `tmp/outreach` earlier in the same
shell makes that `cat` produce nothing, and `dm` will happily send the empty string. `test -s` the
draft first. `dm` refuses a pattern matching anything but exactly one person, and that refusal is
never worked around. **Delete each draft once sent**, so the directory is the outstanding queue.
"Send the first N" is directory order unless the user says otherwise.

**`rolodex pull` whoever you sent to, before finishing.** A send writes nothing to the transcript, so
until a pull re-reads it from the platform they still count as cold — and the next campaign would
open them again, which is the one unrecoverable mistake.

**N is a count of messages delivered, not of attempts.** A send can be refused, and a refused person
does not fill a slot: take the next candidate off the ranked list, draft them, and keep going until
N have actually landed or the list runs out. Say how many went and who is left. Stopping at "5 tried,
2 sent" is the failure — the campaign was for five people.

**A refusal says why, and is never retried.** Skool tries every group of ours and prints a line each;
only the campaign's own group answers. `chat request not allowed` is their DMs being off; `423
Locked` is a group with member chat off, noise unless it is the campaign's; `cannot request to
non-member` means they left. `dm` writes the refusal to their file, so `cold` drops them from then on
— seeing somebody refused twice means that write did not happen, and is worth reporting.

`no __NEXT_DATA__ in the served page` is none of the above — the skool cookie went stale and the WAF
answers `202` with an empty body. Delete `~/.local/state/social_networks/skool_cookies.json` and run
again; a stale cookie is worse than no cookie.

## The base message is law

Not a first draft for you to improve.

- **Do not touch its spacing, capitalisation, punctuation or grammar.** Clumsy is a voice, not an
  error.
- **Do not weave personalisation into its sentences.** Additions go **below**, as a new paragraph.
- Global substitutions the user asks for go into `base_msg.md`, so every draft inherits them.

Exactly **one** permitted per-person mutation: **drop a conditional question the evidence already
answers.** If the base asks "you making money from this?" and their transcript shows they plainly
are, or plainly are not, the question reads as not having listened — drop it and the `If yes,` that
hangs off it, and keep the rest of the sentence. Thin evidence is not "plainly"; keep it then.

## The appended paragraph

At most one, prefixed `btw, `, after a blank line. It must be **a question about something they
themselves wrote**. Two shapes work:

- **Did you solve the problem you posted about?** — `btw, did you get the plumbing GMB verification
  sorted?`
- **A question their demonstrated expertise answers**, narrow enough to reply to in one line —
  `btw, is 700 monthly searches still the floor you'd use in the UK?`

If you can derive neither, **append nothing**. That is the right outcome for most people.

**One line, one question, and grep for it.** Confirm the claim sits on a line matching
`\[<handle>/`, then read the replies under that line to fix what it was about. Two failures, both
already made: a question built off the *neighbouring* line, which was somebody else's; and two of
the person's own lines, weeks apart, welded into one claim neither made. The second passes an
authorship check, so authorship is not the test. The tell for both: the draft credits somebody with
a *deliberate method* when they were describing a circumstance. If they would answer "that is not
what I said", it is this bug.

An earlier campaign's draft is not evidence either — re-derive it against the transcript.

If somebody is plainly a heavy operator and you still cannot derive an ask, put a literal `TODO:` in
their file and surface the raw quotes for the user. Do not invent one. But do not reach for `TODO:`
because your ask feels too small for them: a narrow question that is easy to answer beats a grand
one that is not.

## Style

Campaign-independent, and mostly things not to do. The failure mode is uniform: text that reads as
written by an AI demonstrating that it read carefully.

**Never:**

- **Rhetorical contrast pairs** — `somebody who'd actually done it rather than theorised about it`.
  The most frequent tell, and the reliable symptom of a welded `btw`: the two sources become the two
  horns of an X-not-Y or X-or-just-Y sentence. If a line has that shape, delete it.
- **Ranking them against the group** — `the only one in that group who…`. Flattery that also proves
  you surveilled everybody.
- **Explaining why you are asking** — `that's the part I'd rather learn than rediscover`. Ask, stop.
- **Pitches, offers, or anything committing the user to a future action.** Only the user pitches.
- **Em dashes.** The voice uses `, - `. Match the base.
- **Quoting them at length.** One clause of reference; a verbatim block reads like surveillance.

**Do:** get to the point in the first clause; one or two sentences; match the base's register
exactly; plainest possible phrasing. The test: would the user have typed something this long?

## Judgement calibration

One unfiltered campaign of 26: **20 got the base verbatim**, six an appended question, two of those
a `TODO:`. Over an unfiltered cohort, more than a quarter deviating means you are personalising off
platitudes — go back and cut. A cohort already cut to the loudest few inverts this, and can
legitimately come out five for five. **State which cohort your ratio is over**; a run that does not
has not checked itself.
