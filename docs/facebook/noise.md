# Noise: browsing that is not the work

Each point is **observed** (on the burner, 2026-10-06, with `examples/facebook_noise.rs`), **known** or **built**, as in `sending.md`.

## Why
- **known**: a session that only ever loads a conversation, types, presses Enter and closes is a shape no person has. `send --noise <min>..<max>` browses for a uniform pick of that many seconds after each message, in the same chrome.

## What it may touch (built)
- Links, clicked with the pointer, and the `Search Facebook` field, typed into; nothing else. No button is pressed, so no like, follow, friend request, comment draft or docked chat is ever made: everything it leaves is DOM, gone with the next load by URL, which is how every command starts.
- A link is followed only to a person (`profile.php?id=`) or a page (one path segment outside a reserved set: `groups`, `photo`, `watch`, `reel`, `events`, `friends`, …).
- After each step a visible `[role=dialog]` sends the tab back to the feed by URL. `Tab::landed` runs after every click and search, so a checkpoint, a block or a logout stops it as it stops any load.
- Every load and wheel goes through the session's `behaviour`, caps and breaks included; whatever is under way at the deadline is cut there. Never on the attached session.

## Steps (built)
- **feed** (12/20): back to the feed by URL if elsewhere, then 3–12 wheel gestures in two modes that each hold for a while (switch 1/4 per gesture): skimming, 1–4 notches; reading, the pointer travelled onto a post's text wholly on screen, 1–2 notches, then a 5–25 s pause.
- **someone** (5/20): a person or page linked from a post on screen, clicked; 1–5 gestures there. The feed when none is on screen.
- **search** (3/20): a mundane French/Lyon query typed into the search field, Enter; 2–6 gestures over the results.

## The page (observed)
- Feed posts are `[aria-posinset]`; `[role=article]` holds almost none of them. A post's own text is `[data-ad-preview="message"]` or `[data-ad-comet-preview="message"]`.
- The headless viewport is 513 px tall, so a post's text is often not wholly on screen and reading falls back to skimming.
- Links in posts carry `__cft__`/`__tn__` tracking parameters; page vanity names can hold `-` (`/333-255649711134087/`).
- The feed's top holds the "Create a post" region (a link to the account's own timeline) and "People you may know" (`/friends/suggestions/`), outside `[aria-posinset]`.
- Search lands on `/search/top?q=`, whose results are `[aria-posinset]` posts as well.

## Unverified
- whether a hovercard opened by the pointer resting on a name is a `[role=dialog]`: if so, it costs a feed load, nothing else.
- the `within` wheel finds its target, waits for it and scrolls it into view inside the 10 s a wheel has to ack; a post virtualized away in between would fail the noise as an unacked wheel. Seen with the earlier `[role=article] [dir=auto]` target, not with the current one.
