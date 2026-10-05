# Sending over Messenger

Nothing here is observed yet: no message has gone out through this path. Each point is **known** (general behaviour) or **built** (what the code does).

## Risk
- **known**: a message to a non-friend lands in their Message Requests. New or low-history accounts that send many requests to strangers get "You can't send messages right now" / a temporary block, and repeated, a checkpoint. Facebook publishes no threshold.
- **known**: identical text to many people is a spam signal. Drafts written per person are less exposed than one template.
- **built**: the daily cap is `circuit_breakers.per_surface.facebook`, which `send` refuses to run without. It is set low on purpose: it is the only volume control, since the session's `behaviour` paces page loads, not messages.
- **built**: one `send` run goes through people back to back. Each person costs a conversation load plus ~1 min of typing, so a full day's budget leaves in a few minutes, not spread over the day.

## As built
- Opened by URL, `messages/t/<profile id>`; the composer (`[role=textbox][aria-label=Message]`) is clicked into, typed at ~180 ms a key with the odd corrected typo, and sent with Enter.
- A bubble cannot hold a line break (Enter sends), so a multi-paragraph message needs `--multi-message`, which sends each paragraph as its own bubble 2–6 s apart.
- Sent = the composer is empty again and the conversation shows the text once more than before. A refusal phrase (`you can't message`, `unavailable on messenger`, …) is `Unreachable` on the person; a limit or `not sent` phrase stops the run.

## Unverified until the first real send
- the composer selector and that the page has a `[role=main]` around the conversation;
- that `/messages/t/<id>` opens a new conversation with a non-friend rather than a chooser or an end-to-end-encryption PIN prompt (either times out as an error);
- the wording of refusals and limits: an unlisted wording times out as an error rather than passing, but it is not recorded as `Unreachable` either.

`social_networks_adapters/examples/facebook_messenger_probe.rs` prints the first two off the account's conversation with itself, typing nothing.
