//! A message, from the send session: the conversation is opened by URL, the composer clicked into and
//! typed in, and Enter pressed. It counts as sent once the conversation shows it and the composer is
//! empty again; anything else the page says is a refusal or an error, never a pass.

use std::{path::Path, time::Duration};

use color_eyre::eyre::{Result, bail, ensure};
use serde::Deserialize;

use super::{FEED, Facebook, Session};
use crate::reach::{Direct, Page, Unreachable, Window};

const COMPOSER: &str = r#"[role="textbox"][contenteditable="true"][aria-label="Message"]"#;
/// How long the page gets to show the composer, and then the message sent.
const SETTLE: Duration = Duration::from_secs(30);
/// About them: nothing from this account reaches them.
const REFUSED: &[&str] = &[
	"you can't message",
	"can't reply to this conversation",
	"isn't available on messenger",
	"not available on messenger",
	"unavailable on messenger",
	"isn't receiving messages",
];
/// About us, or about nobody we can tell: a send after it would meet the same.
const FAILED: &[&str] = &[
	"message request limit",
	"reached the limit",
	"limit how often",
	"going too fast",
	"not sent",
	"failed to send",
	"couldn't send",
	"couldn't be sent",
	"didn't send",
];
/// `shown`: how often the message's letters and digits run in the conversation, which is how it is
/// recognised whatever the bubble renders emoji and line breaks as; `null` without a `main`.
const SEE: &str = r#"([composer, message]) => {
	const letters = s => s.toLowerCase().replace(/[^\p{L}\p{N}]/gu, '');
	const boxes = document.querySelectorAll(composer);
	const main = document.querySelector('[role="main"]');
	return {
		composers: boxes.length,
		draft: boxes[0]?.textContent ?? '',
		text: document.body.innerText.toLowerCase().replaceAll('’', "'"),
		shown: main ? letters(main.innerText).split(letters(message)).length - 1 : null,
	};
}"#;

#[derive(Deserialize)]
struct Seen {
	composers: usize,
	draft: String,
	text: String,
	shown: Option<usize>,
}

impl Facebook<'_, '_> {
	/// The page, polled until `done` holds of it or it says a marker.
	async fn watch(&mut self, message: &str, what: &str, done: impl Fn(&Seen) -> bool) -> Result<Seen> {
		let deadline = tokio::time::Instant::now() + SETTLE;
		loop {
			let seen: Seen = self.tab.see(SEE, (COMPOSER, message)).await?;
			if let Some(m) = REFUSED.iter().find(|m| seen.text.contains(**m)) {
				return Err(Unreachable(format!("facebook says \"{m}\"")).into());
			}
			if let Some(m) = FAILED.iter().find(|m| seen.text.contains(**m)) {
				bail!("facebook says \"{m}\"; stop and look at the conversation before sending again");
			}
			if done(&seen) {
				return Ok(seen);
			}
			ensure!(tokio::time::Instant::now() < deadline, "{what} in {SETTLE:?}");
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
	}
}

impl Direct for Facebook<'_, '_> {
	async fn direct(&mut self, _: &str, _: Window, _: &Path) -> Result<Page> {
		bail!("facebook conversations are not read")
	}

	async fn send(&mut self, handle: &str, text: &str) -> Result<()> {
		ensure!(self.session != Session::Attached, "nothing is sent from the user's own facebook");
		ensure!(
			!handle.is_empty() && handle.bytes().all(|b| b.is_ascii_digit()),
			"a facebook handle is the numeric profile id, got `{handle}`"
		);
		ensure!(
			!text.contains('\n'),
			"Enter sends a facebook message, so a line break in one would cut it short; split it with --multi-message"
		);
		ensure!(text.chars().any(char::is_alphanumeric), "`{text}` has no letters to recognise it by once sent");

		match self.conversation.as_deref() == Some(handle) {
			true => tokio::time::sleep(Duration::from_secs_f64(rand::random_range(2.0..6.0))).await, // the next bubble of a burst
			false => self.load(&format!("https://www.facebook.com/messages/t/{handle}"), &[FEED]).await?,
		}
		let open = self.watch(text, "no Messenger composer showed", |s| s.composers > 0).await?;
		ensure!(open.composers == 1, "{} Messenger composers on the page", open.composers);
		ensure!(open.draft.trim().is_empty(), "the composer to {handle} already holds `{}`", open.draft);
		let before = open
			.shown
			.ok_or_else(|| color_eyre::eyre::eyre!("the conversation with {handle} has no `main` to read it from"))?;

		self.tab.type_into(COMPOSER, text).await?;
		self.tab.press(COMPOSER, "Enter").await?;
		self.watch(text, "the message was typed and Enter pressed, but the conversation does not show it sent", |s| {
			s.draft.trim().is_empty() && s.shown.is_some_and(|n| n > before)
		})
		.await?;
		self.conversation = Some(handle.to_string());
		Ok(())
	}
}
