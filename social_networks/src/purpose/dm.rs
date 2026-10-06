//! The one path that writes to a platform rather than reading from it. Addressed by person, so their
//! directory stays the thing you name and the handle is looked up rather than typed.

use clap::Args;
use color_eyre::eyre::{Result, bail, eyre};
use colored::Colorize as _;
use jiff::Timestamp;
use regex::Regex;
use social_networks_adapters::{
	facebook,
	reach::{Author, Direct, Item, Kind, Source, Unreachable},
	skool::Skool,
	telegram_dms, twitter,
};
use social_networks_reach::{
	history,
	person::{self, Person},
	purpose::Purpose,
};
use social_networks_utils::db::Database;
use strum::{AsRefStr, EnumString};

use super::with_telegram;
use crate::config::AppConfig;

/// Clap has no flag-to-enum, so the group is how [`Messenger`] is spelled on the command line.
#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct MessengerFlag {
	#[arg(long)]
	discord: bool,
	#[arg(long)]
	facebook: bool,
	#[arg(long)]
	skool: bool,
	#[arg(long)]
	telegram: bool,
	#[arg(long)]
	twitter: bool,
}
impl From<&MessengerFlag> for Messenger {
	fn from(flag: &MessengerFlag) -> Self {
		match (flag.discord, flag.facebook, flag.skool, flag.telegram, flag.twitter) {
			(true, ..) => Self::Discord,
			(_, true, ..) => Self::Facebook,
			(_, _, true, ..) => Self::Skool,
			(_, _, _, true, _) => Self::Telegram,
			(.., true) => Self::Twitter,
			_ => unreachable!("clap rejects the command before this when the group is unfilled"),
		}
	}
}

/// `as_ref` is the `handles` key, so a messenger cannot be reachable under a name the person files
/// do not use. It also names the person's `outbox/<messenger>/` directory.
#[derive(AsRefStr, Clone, Copy, Debug, EnumString, Eq, Ord, PartialEq, PartialOrd)]
#[strum(serialize_all = "lowercase")]
pub enum Messenger {
	Discord,
	Facebook,
	Skool,
	Telegram,
	Twitter,
}

/// Exactly one person: `pull` over an ambiguous pattern costs a wasted fetch, a DM over one goes to
/// the wrong human and cannot be taken back.
pub async fn send(config: &AppConfig, purpose: &Purpose, messenger: Messenger, pattern: &str, text: &str, multi_message: bool) -> Result<()> {
	let dir = &purpose.path;
	let people = person::load_dir(purpose)?;
	let matches: Vec<&Person> = people.values().filter(|p| p.matches(pattern)).collect();
	let [person] = matches[..] else {
		bail!(
			"`{pattern}` matches {} in {}: {}",
			matches.len(),
			dir.display(),
			matches.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
		);
	};
	send_to(config, purpose, person, messenger, text, multi_message).await
}

/// An `Unreachable` refusal is recorded on the person before it is returned, and so is every bubble
/// that went out, where no pull reads it back. `multi_message` sends
/// `text` split on blank lines as consecutive bubbles; they are one message to the breaker.
pub async fn send_to(config: &AppConfig, purpose: &Purpose, person: &Person, messenger: Messenger, text: &str, multi_message: bool) -> Result<()> {
	let dir = &purpose.path;
	let platform = messenger.as_ref();
	let handle = person.handles.get(platform).ok_or_else(|| eyre!("{} has no {platform} handle", person.name))?;
	let bubbles: Vec<&str> = match multi_message {
		true => Regex::new(r"\n\n+").expect("literal pattern").split(text).map(str::trim).collect(),
		false => vec![text],
	};
	if let Some(i) = bubbles.iter().position(|b| b.is_empty()) {
		bail!("bubble {} of {} to {} is empty", i + 1, bubbles.len(), person.name);
	}
	config.circuit_breakers.admit(&Database::try_new().await?, &format!("{platform}:{handle}")).await?;

	// one `Direct::send`, a session per messenger: the same enum dispatch the reads go through
	let mut sent_at = Vec::with_capacity(bubbles.len());
	let sent = match messenger {
		Messenger::Discord =>
			burst(
				&mut social_networks_adapters::discord::Rest::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone()),
				handle,
				&bubbles,
				&mut sent_at,
			)
			.await,
		Messenger::Facebook => {
			let fb = config
				.facebook
				.as_ref()
				.ok_or_else(|| eyre!("a facebook message goes out from a logged-in chrome, so it needs a `facebook` section in the config"))?;
			facebook::with_sender(fb, async |session| burst(session, handle, &bubbles, &mut sent_at).await).await
		}
		// the read path is happy anonymous, but a message is written as somebody
		Messenger::Skool => {
			let credentials = config
				.skool
				.as_ref()
				.ok_or_else(|| eyre!("sending a skool DM signs in, so it needs a `[skool]` section in the config"))?;
			burst(&mut Skool::try_new(Some(credentials.clone())).await?, handle, &bubbles, &mut sent_at).await
		}
		Messenger::Telegram =>
			with_telegram(&config.telegram, async |client| {
				burst(&mut telegram_dms::Reach { client: &client }, handle, &bubbles, &mut sent_at).await
			})
			.await,
		Messenger::Twitter => burst(&mut twitter::Reach(&config.twitter), handle, &bubbles, &mut sent_at).await,
	};

	// The outcome is worth as much as the message: a campaign that does not record a refusal picks
	// the same person again next time and spends another request learning the same thing. Only an
	// `Unreachable` counts — a dead session or a dropped connection is ours, not theirs.
	let mut person = person.clone();
	let was = person.unreachable.remove(platform);
	if let Err(e) = &sent
		&& let Some(refusal) = e.downcast_ref::<Unreachable>()
	{
		person.unreachable.insert(platform.to_string(), refusal.to_string());
	}
	if was != person.unreachable.get(platform).cloned() {
		person.write(dir)?;
	}
	let read_back = match messenger {
		Messenger::Facebook => Some(Source::Facebook),
		Messenger::Discord | Messenger::Skool | Messenger::Telegram => None, // a pull reads our side back
		Messenger::Twitter => None,                                          // keeps no transcript
	};
	if let Some(source) = read_back
		&& !sent_at.is_empty()
	{
		let items: Vec<Item> = bubbles
			.iter()
			.zip(&sent_at)
			.map(|(bubble, at)| Item {
				id: format!("sent:{at}"),
				source,
				at: *at,
				kind: Kind::Direct,
				author: Author::Me,
				text: bubble.to_string(),
				attachments: Vec::new(),
				permalink: None,
			})
			.collect();
		let person_dir = person.dir(dir);
		let mut meta = history::Meta::load(&person_dir)?;
		let mut cursor = meta.cursor(source)?;
		match cursor.archiving() {
			true => cursor.stash(&items)?,
			false => history::record(&person_dir, items, &mut meta)?,
		}
	}
	sent?;

	println!("   {} {platform}/{handle} ({})", "✓".green(), person.name);
	Ok(())
}

/// Bubbles already out cannot be taken back, so a burst cut short says how far it got.
/// A burst cut short: `rest` is what did not go out, in the shape `send_to` splits.
#[derive(Debug)]
pub struct PartlySent {
	pub sent: usize,
	pub rest: String,
}
impl std::fmt::Display for PartlySent {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} bubbles already went out", self.sent)
	}
}

async fn burst(session: &mut impl Direct, handle: &str, bubbles: &[&str], sent_at: &mut Vec<Timestamp>) -> Result<()> {
	for (i, bubble) in bubbles.iter().enumerate() {
		if let Err(e) = session.send(handle, bubble).await {
			return match i {
				0 => Err(e),
				_ => Err(e.wrap_err(PartlySent {
					sent: i,
					rest: bubbles[i..].join("\n\n"),
				})),
			};
		}
		sent_at.push(Timestamp::now());
	}
	Ok(())
}
