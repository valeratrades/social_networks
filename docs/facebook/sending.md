# Sending over Messenger

Each point is **observed** (seen on the burner, 2026-10-05/06, with `examples/facebook_messenger_probe.rs`), **known** (general behaviour) or **built** (what the code does). The probe's `--send` goes through `send` itself, to the burner's own conversation or the user's account only.

## Risk
- **known**: a message to a non-friend lands in their Message Requests. New or low-history accounts that send many requests to strangers get "You can't send messages right now" / a temporary block, and repeated, a checkpoint. Facebook publishes no threshold.
- **known**: identical text to many people is a spam signal. Drafts written per person are less exposed than one template.
- **built**: volume is `send -n <N>`, said by the human on every run. The session's `behaviour` paces page loads, not messages.
- **built**: one `send` run goes through people back to back. Each person costs a conversation load plus ~1 min of typing.

## What stands in front of the composer (observed)
- `/messages/t/<profile id>` of a chat that predates end-to-end encryption shows a notice in `[role=main]`, "This chat is now secured with end-to-end encryption" or "These messages were sent before this chat was secured… You can't reply", with one **Continue** button. Continue opens the encrypted chat (its own URL, `/messages/e2ee/t/<thread id>`; the thread id is not the profile id).
- A browser that has not restored the account's encrypted history gets a dialog: "Enter your PIN to restore your chats" (6 digit boxes, **Close**, "Forgot PIN?"). Close asks "Continue without restoring? You won't see your full chat history on this device and new messages you send or receive may not load on other new devices", **Cancel** / **Don't restore messages**. Don't restore is remembered by the browser: the dialog does not come back; a "Chat history is missing — Enter PIN" card stays in the chat list, which blocks nothing.
- The composer is `[role=main] [role=textbox][contenteditable][aria-label^="Write to "]`: "Write to <name>", or "Write to " under a "To:" chip when the chat is with the account itself. `aria-label="Message"` does not exist.
- A conversation with no message in it yet (a stranger's, or the account's own before its first message) is a **new message** view: `main` is labelled "New message", the person is a chip `[role=button][aria-label="Remove <name>"]` in a "To:" row, and the "Send message to" `combobox` is `aria-expanded="true"` with its suggestions ("Your contacts", a `listbox` portalled outside `main`) hanging from under the To row down to the bottom of the viewport. At 800×568 it covers the whole composer, so a click into it lands on a contact, which would add them as a recipient. Focus starts on the chip's Remove button.
- Escape and Tab (the field's hint says "Tab to chat") leave the list open; focusing the composer and typing into it does too. A pointer click on the "To:" label closes it, with the chip intact, and the composer is then clear to click.
- Once its first message is out, a new message view turns into the thread (`/messages/e2ee/t/<thread id>`, header with the name, no To row) a few seconds later, in place, under whatever is being typed.
- A bubble shows the moment Enter is pressed, and "Sent" appears under it ~0.3 s later. A chrome closed in between loses the message: the conversation shows it nowhere afterwards.

## As built
- The conversation is opened by URL, then `messenger.js` reads the topmost visible dialog, or without a composer the notice in `main`, and the first button it offers from a fixed list, most specific first: Don't restore messages, Skip, Not now, Continue, OK, Got it, Dismiss, Decline optional cookies, Close. That button is clicked with the pointer, after a `behaviour` load, and the page is read again, up to 8 times, until the composer shows with nothing in front of it. A PIN dialog with no button from the list is the human's (`recon facebook-login`, enter the PIN); any other dialog or notice without one times out as an error that quotes it. "Forgot PIN?" and Cancel are never clicked.
- On a new message view, the "To:" label is clicked, and the suggestions must close with the recipients (the chip, and the composer's `Write to …`) as they were. Then there must be one composer, at most one chip, and nothing over the composer at any point a click into it can land on.
- The composer is clicked into, typed at ~180 ms a key with the odd corrected typo; then the recipients must still be the ones read before typing and the composer must hold the text, or nothing is sent. Enter sends.
- A bubble cannot hold a line break (Enter sends), so a multi-paragraph message needs `--multi-message`, which sends each paragraph as its own bubble 2–6 s apart.
- Sent = the composer is empty again, the conversation shows the text once more than before, and the line under its last bubble starts with "Sent" or "Delivered".
- A burst's next bubble after a new message view's first one loads the conversation afresh, since that view is about to turn into the thread. A refusal phrase (`you can't message`, `unavailable on messenger`, …) is `Unreachable` on the person; a limit or `not sent` phrase stops the run.

## Unverified
- the wording of refusals and limits: an unlisted wording times out as an error rather than passing, but it is not recorded as `Unreachable` either.
- a message the recipient sees within the second it goes out shows "Seen" under it rather than "Sent": that times out as an error after it was sent.
