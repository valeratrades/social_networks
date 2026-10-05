# Sending over Messenger

Each point is **observed** (seen on the burner, 2026-10-05, with `examples/facebook_messenger_probe.rs`), **known** (general behaviour) or **built** (what the code does). No message has gone out through this path yet.

## Risk
- **known**: a message to a non-friend lands in their Message Requests. New or low-history accounts that send many requests to strangers get "You can't send messages right now" / a temporary block, and repeated, a checkpoint. Facebook publishes no threshold.
- **known**: identical text to many people is a spam signal. Drafts written per person are less exposed than one template.
- **built**: volume is `send -n <N>`, said by the human on every run. The session's `behaviour` paces page loads, not messages.
- **built**: one `send` run goes through people back to back. Each person costs a conversation load plus ~1 min of typing.

## What stands in front of the composer (observed)
- `/messages/t/<profile id>` of a chat that predates end-to-end encryption shows a notice in `[role=main]`, "This chat is now secured with end-to-end encryption" or "These messages were sent before this chat was secured… You can't reply", with one **Continue** button. Continue opens the encrypted chat (its own URL, `/messages/e2ee/t/<thread id>`; the thread id is not the profile id).
- A browser that has not restored the account's encrypted history gets a dialog: "Enter your PIN to restore your chats" (6 digit boxes, **Close**, "Forgot PIN?"). Close asks "Continue without restoring? You won't see your full chat history on this device and new messages you send or receive may not load on other new devices", **Cancel** / **Don't restore messages**. Don't restore is remembered by the browser: the dialog does not come back; a "Chat history is missing — Enter PIN" card stays in the chat list, which blocks nothing.
- The composer is `[role=main] [role=textbox][contenteditable][aria-label^="Write to "]`: "Write to <name>", or "Write to " under a "To:" chip when the chat is with the account itself. `aria-label="Message"` does not exist.

## As built
- The conversation is opened by URL, then `messenger.js` reads the topmost visible dialog, or without a composer the notice in `main`, and the first button it offers from a fixed list, most specific first: Don't restore messages, Skip, Not now, Continue, OK, Got it, Dismiss, Decline optional cookies, Close. That button is clicked with the pointer, after a `behaviour` load, and the page is read again, up to 8 times, until the composer shows with nothing in front of it. A PIN dialog with no button from the list is the human's (`recon facebook-login`, enter the PIN); any other dialog or notice without one times out as an error that quotes it. "Forgot PIN?" and Cancel are never clicked.
- The composer is clicked into, typed at ~180 ms a key with the odd corrected typo, and sent with Enter.
- A bubble cannot hold a line break (Enter sends), so a multi-paragraph message needs `--multi-message`, which sends each paragraph as its own bubble 2–6 s apart.
- Sent = the composer is empty again and the conversation shows the text once more than before. A refusal phrase (`you can't message`, `unavailable on messenger`, …) is `Unreachable` on the person; a limit or `not sent` phrase stops the run.

## Unverified until the first real send
- what a stranger with no chat yet gets at `/messages/t/<id>`: a new chat, a "To:" compose view, or a prompt not in the list (which errors, quoted);
- the wording of refusals and limits: an unlisted wording times out as an error rather than passing, but it is not recorded as `Unreachable` either.
