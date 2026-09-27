# Facebook detection surface and browser options

Facts from the 2026-09-22 live work, plus known general behaviour; each is marked **observed** (seen on this account/machine) or **known** (general knowledge, not tested here). Thresholds are unpublished by facebook.

## What facebook can see, by layer

```
 layer                     signals                                          what tooling can change
 ─────────────────────────────────────────────────────────────────────────────────────────────────────────
 4. behaviour (account)    views/hour, profile→profile with no feed use,    nothing; only pacing and volume
                           round-the-clock activity, repetitive pattern
 3. session / device       cookie history, known device, login location     real profile keeps it; a fresh
                                                                            profile starts from zero
 2. browser fingerprint    navigator.webdriver, CDP leaks (Runtime.enable), real Chrome: genuine; Playwright:
                           headless tells, WebGL/GPU, fonts, TLS            flagged by default; antidetect:
                                                                            spoofed
 1. network                IP reputation, datacenter vs residential, geo    home IP genuine; proxies vary
```

## Observed
- A fresh Chrome profile (own `--user-data-dir`, no automation flags) got facebook's "are you human" check on its first login.
- On the daily Chrome (CDP port 49300), a session made by a new login died server-side within ~5–50 min while other CDP clients were attached. Only facebook.com cookies went; `datr` was re-issued. Cause unconfirmed: `tmp/ongoing_debug/2026-09-22_facebook-session-drop.md`.
- Non-default Chrome profiles (e.g. "Profile 5", evinvest.ltd) are not CDP browser contexts: `Storage.getCookies` / `Target.createTarget` with their `browserContextId` fail. A tab in them can be attached to directly, and `Network.getCookies` on that session works.
- `Input.dispatchMouseEvent` is acked only when the window is rendered. On a hidden sway workspace no frames are produced, so wheel events hang and `Page.startScreencast` delivers 0 frames. `document.visibilityState` still reports `visible` there. Page loads and `Runtime.evaluate` work while hidden.
- A window moved (`swaymsg '[con_id=…] move container to workspace <ws>'`) onto a workspace of a `swaymsg create_output` headless output gets frames: wheel events are acked and pagination fires, with no visible window and the focused workspace unchanged (2026-09-24).
- With the window visible, wheel scrolling on `/groups/<id>/members` triggers GraphQL pagination: 10 → 25 → 35 members.
- The nix `google-chrome-stable` wrapper always injects the daily CDP flags (`--remote-debugging-port=49300`, `--user-data-dir=~/.config/google-chrome-cdp`, `--silent-debugger-extension-api`, `--disable-field-trial-config`, …). Chrome takes the last occurrence of a repeated flag, so extra args override port and profile. The other injected flags stay.
- A fresh profile on this machine gets a set of policy-installed extensions (dark reader, tampermonkey, …).
- The About tab moved to `directory_*` sections (`directory_personal_details`, `_work`, `_education`, `_contact_info`, `_names`). `about_places` and `about_work_and_education` load no field data.

## Known (not tested here)
- chromiumoxide, Puppeteer and Playwright send `Runtime.enable` on attach. That's the best-known CDP-detection signal (console serialization side effects).
- Playwright's own launch sets `--enable-automation` (`navigator.webdriver = true`) and uses a fresh profile unless given a persistent context. `connectOverCDP` to an existing Chrome keeps the profile but still sends `Runtime.enable`.
- Patchright or "stealth" forks remove webdriver and `Runtime.enable`. The profile is still fresh.
- Headless Chrome: `HeadlessChrome` in the UA unless overridden. Without a GPU, WebGL reports SwiftShader/llvmpipe.
- Antidetect browsers (GoLogin/Orbita, Multilogin, AdsPower): a modified Chromium with spoofed, self-consistent fingerprints and a proxy per profile, driven over CDP. Their use case is many accounts that must look unlinked. Facebook actively targets fingerprint spoofing.
- The typical response to bursts of profile views is "You're going too fast" / "You can't use this feature right now", for hours to days. Repeated, it escalates to a checkpoint or account restrictions.

## Throughput as built
- 1 profile view per member (`directory_personal_details`), plus 0–3 per in-area member (work, education, contact info when present).
- At `profile_views_per_hour = 30`: 720 views/day, so a 10k-member group is ~14 days.
- Member listing: ~10 members per scroll. A 10k group is ~1000 scrolls, and the window must be visible throughout.

## Measured (2026-09-24, group 671440486248842 "Colocation Lyon", `--location Lyon`, `examples/measure.rs`)
- Members embedded in the first page: 15 (4.6 s). Scrolling adds ~10 per scroll.
- `directory_personal_details` load + parse: 3.1 s/visit. With 5–20 s pauses: 4.6 visits/min.
- Living in Lyon: 1/15 (6.7 %, 95 % CI ≈ 1–30 %). No public current city: 9/15. Others: Paris ×3, Warsaw, Brussels.
- At 30 views/h: ≈ 0.03 Lyon residents/min (≈ 2/h).

## People search with the City filter (2026-09-24, observed)
- URL: `https://www.facebook.com/search/people/?q=<name>&filters=<base64>`, the base64 of `{"city:0":"{\"name\":\"users_location\",\"args\":\"<city page id>\"}"}`. Lyon = `108560402508141`.
- The first page embeds `serpResponse.results`: 10 `edges`, each with `rendering_strategy.view_model` (`__typename: SearchProfileViewModel`), whose `profile` (`__typename: User`) has `id`, `name`, `url`, `profile_picture.uri`. The lines under the name are `primary_snippet_text_with_entities.text` (also `prominent_…`, `description_snippets_…[]`), e.g. "Works at … · Lives in Lyon, France".
- Each scroll's GraphQL answer is `{"data":{"serpResponse":{"results":…}}}` of the same shape, 10–11 more edges.
- `results.page_info.has_next_page` says whether scrolling loads more. `results.filters[].filters[].main_filter` with `name: "city"` carries `current_value: {text: "Lyon, France", value: "{\"name\":\"users_location\",\"args\":\"108560402508141\"}"}`.
- A query's result list is capped, so `city` walks first names. Names: INSEE *Fichier des prénoms* `nat2022` (https://www.insee.fr/fr/statistiques/fichier/7633685/nat2022_csv.zip), births ≥ 1945, `_PRENOMS_RARES` dropped, lowercased and summed over sexes, the top 2000 by count (92.1 % of those births), most frequent first.

## Measured (2026-09-24, `facebook city --location Lyon 108560402508141`, natural account, window parked on a headless output)
- Query `jean` did not run out: 701 distinct people over ~65 scrolls in ~14 min (≈ 11 per scroll, ≈ 50/min), then the 120 scrolls/h cap. Capped steady state ≈ 20 people/min.
- Their snippets: "Lives in …Lyon…" 335 (48 %), "Lives in" elsewhere 245 (35 %), no place 103 (15 %), Lyon mentioned otherwise 18 (3 %).
