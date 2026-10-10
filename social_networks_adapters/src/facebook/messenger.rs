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
	reach::{Direct, Page, Refusal, Unreachable, Window},
};

const COMPOSER: &str = r#"[role="main"] [role="textbox"][contenteditable="true"][aria-label^="Write to "]"#;
const SEE: &str = include_str!("messenger.js");
/// How long the page gets to show the composer, or something in front of it, and then the message sent.
const SETTLE: Duration = Duration::from_secs(30);
/// Prompts clicked through on the way to one composer; past it, they are not going away.
const PASSES: usize = 8;
/// What the line under a bubble starts with once it went out.
const SENT: &[&str] = &["Sent", "Delivered"];
/// About them: nothing from this account reaches them.
const REFUSED: &[(&str, Refusal)] = &[
	("you can't message", Refusal::Closed), // "you can't message this account. it may help to add them as a friend on facebook."
	("can't reply to this conversation", Refusal::Closed),
	("isn't receiving messages", Refusal::Closed),
	("isn't available on messenger", Refusal::Absent),
	("not available on messenger", Refusal::Absent),
	("unavailable on messenger", Refusal::Absent),
	("can't access this chat yet", Refusal::Dormant), // "you'll be able to send messages when <name> next logs into messenger"
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
	/// the composer's `Write to <recipients>`
	to: Option<String>,
	/// the To field's chips, on a conversation that is not yet one
	recipients: Vec<String>,
	covered: bool,
	text: String,
	shown: Option<usize>,
	/// the line under the message's last bubble, where Messenger says whether it went out
	status: Option<String>,
	/// the recipient field's contact list, open over the composer of a new conversation
	suggesting: bool,
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
			let seen: Seen = match self.tab.see(SEE, (COMPOSER, message)).await {
				Ok(seen) => seen,
				// a first message moves a new conversation to its `/messages/e2ee/t/<thread>` url mid-read
				Err(e) if e.to_string().contains("Execution context was destroyed") && tokio::time::Instant::now() < deadline => {
					tokio::time::sleep(Duration::from_millis(500)).await;
					continue;
				}
				Err(e) => return Err(e),
			};
			if let Some((m, refusal)) = REFUSED.iter().find(|(m, _)| seen.text.contains(m)) {
				return Err(Unreachable {
					refusal: *refusal,
					said: seen.text.lines().find(|l| l.contains(m)).expect("a marker holds no line break").trim().to_string(),
				}
				.into());
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
			if passed > 0 {
				self.behaviour.act(Action::Load).await?; // paced before the read, not between it and the click: a notice can go away by itself meanwhile
			}
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
			let (button, pass) = (front.button(pass), pass.clone());
			if let Err(e) = self.tab.click(&button).await {
				let now: Seen = self.tab.see(SEE, (COMPOSER, message)).await?;
				if now.front.and_then(|f| f.pass).as_ref() == Some(&pass) {
					return Err(e);
				}
				info!("`{pass}` went away by itself before the click landed"); // notices dismiss themselves once the conversation loads
			}
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
		let mut open = self.composer(handle, text).await?;
		if open.suggesting {
			// the list hangs over the composer and Escape or Tab leave it open; a click outside closes it
			self.tab.click(r#"[role="main"] span:text-is("To:") >> nth=0"#).await?;
			let seen = self
				.watch(text, "the recipient suggestions over the composer stayed open after a click on `To:`", |s| !s.suggesting)
				.await?;
			ensure!(
				(&seen.to, &seen.recipients) == (&open.to, &open.recipients),
				"closing the recipient suggestions changed the recipients from {:?} {:?} to {:?} {:?}; look at the conversation",
				open.to,
				open.recipients,
				seen.to,
				seen.recipients
			);
			open = seen;
		}
		ensure!(open.composers == 1, "{} Messenger composers on the page", open.composers);
		ensure!(open.recipients.len() <= 1, "the conversation with {handle} is addressed to {:?}", open.recipients);
		ensure!(!open.covered, "something lies over the composer to {handle}, where a click into it would land");
		ensure!(open.draft.trim().is_empty(), "the composer to {handle} already holds `{}`", open.draft);
		let before = open
			.shown
			.ok_or_else(|| color_eyre::eyre::eyre!("the conversation with {handle} has no `main` to read it from"))?;

		self.tab.type_into(COMPOSER, text).await?;
		let typed: Seen = self.tab.see(SEE, (COMPOSER, text)).await?;
		let letters = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>();
		ensure!(
			(&typed.to, &typed.recipients) == (&open.to, &open.recipients),
			"typing changed the recipients from {:?} {:?} to {:?} {:?}; not sending",
			open.to,
			open.recipients,
			typed.to,
			typed.recipients
		);
		ensure!(
			letters(&typed.draft) == letters(text),
			"the composer to {handle} holds `{}` instead of what was typed",
			typed.draft
		);
		self.tab.press(COMPOSER, "Enter").await?;
		// the bubble shows before it goes out, and a chrome closed meanwhile loses it
		self.watch(text, "the message was typed and Enter pressed, but the conversation does not show it sent", |s| {
			s.draft.trim().is_empty() && s.shown.is_some_and(|n| n > before) && s.status.as_deref().is_some_and(|l| SENT.iter().any(|m| l.starts_with(m)))
		})
		.await?;
		// a new conversation turns into its thread under the next bubble's typing, so that one loads it afresh
		self.conversation = open.recipients.is_empty().then(|| handle.to_string());
		Ok(())
	}
}
