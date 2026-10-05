//! A message, from the send session: the conversation is opened by URL, clicked past whatever stands
//! in front of its composer, the composer clicked into and typed in, and Enter pressed. It counts as
//! sent once the conversation shows it and the composer is empty again; anything else the page says is
//! a refusal or an error, never a pass. See `docs/facebook/sending.md`.

use std::{path::Path, time::Duration};

use color_eyre::eyre::{Result, bail, ensure};
use serde::Deserialize;
use tracing::info;

use super::{FEED, Facebook, Session};
use crate::{
	behaviour::Action,
	reach::{Direct, Page, Unreachable, Window},
};

const COMPOSER: &str = r#"[role="main"] [role="textbox"][contenteditable="true"][aria-label^="Write to "]"#;
const SEE: &str = include_str!("messenger.js");
/// How long the page gets to show the composer, or something in front of it, and then the message sent.
const SETTLE: Duration = Duration::from_secs(30);
/// Prompts clicked through on the way to one composer; past it, they are not going away.
const PASSES: usize = 8;
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

#[derive(Deserialize)]
struct Seen {
	composers: usize,
	draft: String,
	text: String,
	shown: Option<usize>,
	front: Option<Front>,
}
/// What stands in front of the composer; see `messenger.js`.
#[derive(Deserialize)]
struct Front {
	kind: Kind,
	text: String,
	buttons: Vec<String>,
	pass: Option<String>,
	pin: bool,
}
impl Front {
	/// `pass` within it; a regex, since facebook writes the apostrophe in "Don't" either way.
	fn button(&self, pass: &str) -> String {
		let scope = match self.kind {
			Kind::Dialog => r#":is([role="dialog"], [role="alertdialog"]):visible >> nth=-1"#,
			Kind::Notice => r#"[role="main"]"#,
		};
		format!("{scope} >> role=button[name=/^{}$/] >> nth=0", pass.replace('\'', "."))
	}
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
	Dialog,
	Notice,
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
			if tokio::time::Instant::now() >= deadline {
				match &seen.front {
					Some(f) => bail!("{what} in {SETTLE:?}; in front of it, a {:?} with buttons {:?}: {}", f.kind, f.buttons, f.text),
					None => bail!("{what} in {SETTLE:?}"),
				}
			}
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
	}

	/// The composer of the conversation the tab is on, once every prompt in front of it is clicked
	/// through. A PIN with no way past it is the human's to enter.
	async fn composer(&mut self, handle: &str, message: &str) -> Result<Seen> {
		for passed in 0.. {
			let seen = self
				.watch(message, "no Messenger composer showed", |s| match &s.front {
					None => s.composers > 0,
					Some(f) => f.pass.is_some() || f.pin,
				})
				.await?;
			let Some(front) = &seen.front else { return Ok(seen) };
			let Some(pass) = &front.pass else {
				let send = match self.session {
					Session::Send => " --send",
					_ => "",
				};
				bail!(
					"Messenger asks this chrome for the account's end-to-end-encryption PIN, with no way past it. Once, by hand: `nix develop -c recon facebook-login{send}`, open facebook.com/messages, enter the PIN, close the window. Its dialog: {}",
					front.text
				);
			};
			ensure!(
				passed < PASSES,
				"clicked through {PASSES} prompts in front of the conversation with {handle}, and more came: {}",
				front.text
			);
			info!("clicking `{pass}` on a {:?} in front of the conversation with {handle}: {}", front.kind, front.text);
			self.behaviour.act(Action::Load).await?; // what it leads to is a page of its own
			self.tab.click(&front.button(pass)).await?;
		}
		unreachable!("an unbounded loop")
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
		let open = self.composer(handle, text).await?;
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
