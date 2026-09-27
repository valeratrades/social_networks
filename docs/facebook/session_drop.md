# Facebook logs the session out while we drive the browser over CDP

Problem: a Facebook login made in the CDP-exposed Chrome does not survive while we scrape. The user has had to log in again repeatedly ("every 4 minutes"). We need one login that lasts, and a known way to recover when it doesn't.

## Facts
- The CDP Chrome (port 49300) is the **daily profile**. `~/.config/google-chrome-cdp` is a bind mount of `~/.config/google-chrome` (`$NIXOS_CONFIG/os/nixos/desktop/services/chrome-cdp.nix`). So a Facebook logout here logs the user out of their real browser.
- Login at 19:31:52 UTC on 2026-09-22 (`c_user` expiry minus 1 y). It had died by the 4th probe run, somewhere in ~5–50 min.
- After the drop, **only facebook.com cookies** were gone. The other 2768 cookies were intact, `datr` had been re-issued with a fresh expiry, and `sb`/`fr` were gone too. So the server logged the session out; nothing local wiped the jar. `chrome_tab_reaper.py` only closes bitunix tabs.
- What had touched facebook.com: 4 page loads of `/groups/<id>/members` in new CDP targets over raw CDP (node, **no** `Runtime.enable`), a few `window.scrollTo`/`scrollBy` calls via `Runtime.evaluate`, `Page.bringToFront`, and `Network.getResponseBody`.
- chromiumoxide 0.9.1 sends `Runtime.enable` to every attached page (`handler/frame.rs` `init_commands`, not configurable). `Browser::connect` attaches to all existing tabs. Launch `DEFAULT_ARGS` include `--enable-automation` (`navigator.webdriver = true`).
- `window.scrollTo` did not trigger member-list pagination. No `GroupsCometMembers…` GraphQL call fired, and `scrollY` snapped back to 0.

## Hypotheses (unconfirmed)
1. Detection of an automation signal: `Runtime.enable` from another CDP client (other agents), or the synthetic scrolling.
2. A behaviour trigger: the same members page loaded repeatedly in new tabs right after a fresh login.

## Mitigation in place (rev. 2, 2026-09-22)
- A fresh own-profile Chrome got facebook's "are you human" check on login, so that was dropped. The scraper attaches to the daily Chrome (`cdp_port`) and takes over its single facebook tab (evinvest profile), through a hand-rolled CDP client that never sends `Runtime.enable`. Scrolling is `Input.dispatchMouseEvent` wheel events.
- The earlier finding "`window.scrollTo` does not paginate" was probably the window sitting on a hidden sway workspace: no frames there, so no IntersectionObserver and no input acks. Wheel scrolling paginates when the window is visible (35 members seen).
- Each drop appends a line to `$XDG_STATE_HOME/social_networks/facebook/{attached,launched}/sessions.toml`: `logged_in_at`, `lifetime`, `page_views`, `scrolls`, `last_url`. Use it to tell hypothesis 1 from 2: a drop with 0 scrolls and few views points to presence detection; drops that grow with views or scrolls point to behaviour.

## Drops observed with the new session
(none yet)
