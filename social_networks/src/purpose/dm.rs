//! The one path that writes to a platform rather than reading from it. Addressed by person, so their
//! directory stays the thing you name and the handle is looked up rather than typed.

use std::{ops::RangeInclusive, str::FromStr, time::Duration};

use clap::Args;
use color_eyre::eyre::{Result, bail, ensure, eyre};
use colored::Colorize as _;
use jiff::Timestamp;
use regex::Regex;
use social_networks_adapters::{
	facebook,
	reach::{Author, Browsing as _, Direct, Item, Kind, Source, Unreachable},
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

/// `--noise <min>..<max>`: seconds of idle browsing after a message, picked uniformly per person.
#[derive(Clone, Debug)]
pub struct Noise(RangeInclusive<f64>);
impl Noise {
	pub fn pick(&self) -> Duration {
		Duration::from_secs_f64(rand::random_range(self.0.clone()))
	}
}

impl FromStr for Noise {
	type Err = color_eyre::Report;

	fn from_str(s: &str) -> Result<Self> {
		let (min, max) = s.split_once("..").ok_or_else(|| eyre!("noise is `<min>..<max>` seconds, got `{s}`"))?;
		let (min, max): (f64, f64) = (min.parse()?, max.parse()?);
		ensure!(0. <= min && min <= max, "noise is `<min>..<max>` seconds with 0 ≤ min ≤ max, got `{s}`");
		Ok(Self(min..=max))
	}
}

/// What is browsed in the chrome a message goes out from, before it and after it.
#[derive(Clone, Copy)]
pub struct Browse {
	pub before: Option<Duration>,
	pub after: Option<Duration>,
}

/// Exactly one person: `pull` over an ambiguous pattern costs a wasted fetch, a DM over one goes to
/// the wrong human and cannot be taken back.
pub async fn send(config: &AppConfig, purpose: &Purpose, messenger: Messenger, pattern: &str, text: &str, multi_message: bool, noise: Option<&Noise>) -> Result<()> {
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
	let browse = Browse {
		before: None,
		after: noise.map(Noise::pick),
	};
	send_to(config, purpose, person, messenger, text, multi_message, browse).await?
}

/// An `Unreachable` refusal is recorded on the person before it is returned, and so is every bubble
/// that went out, where no pull reads it back. `multi_message` sends
/// `text` split on blank lines as consecutive bubbles; they are one message to the breaker.
///
/// The outer error is the message's: it did not all go out. The inner one is whatever failed once it
/// had — the noise browsed after it, the browser closing.
pub async fn send_to(config: &AppConfig, purpose: &Purpose, person: &Person, messenger: Messenger, text: &str, multi_message: bool, browse: Browse) -> Result<Result<()>> {
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
	if (browse.before.is_some() || browse.after.is_some()) && messenger != Messenger::Facebook {
		bail!("browsing happens in the chrome the message goes out from, and {platform} sends through none");
	}
	config.circuit_breakers.admit(&Database::try_new().await?, &format!("{platform}:{handle}")).await?;

	// one `Direct::send`, a session per messenger: the same enum dispatch the reads go through
	let mut sent_at = Vec::with_capacity(bubbles.len());
	let r = match messenger {
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
			facebook::with_sender(fb, async |session| {
				if let Some(span) = browse.before {
					session.noise(span).await?;
				}
				burst(session, handle, &bubbles, &mut sent_at).await?;
				match browse.after {
					Some(span) => session.noise(span).await,
					None => Ok(()),
				}
			})
			.await
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

	// by what went out rather than by `r`: a Ctrl-C or a failure past the last bubble must not send it again
	let sent = match sent_at.len() == bubbles.len() {
		true => Ok(r),
		false => {
			let e = r.expect_err("a burst returns Ok only once every bubble went out");
			Err(match sent_at.len() {
				0 => e,
				i => e.wrap_err(PartlySent {
					sent: i,
					rest: bubbles[i..].join("\n\n"),
				}),
			})
		}
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
	let after = sent?;

	println!("   {} {platform}/{handle} ({})", "✓".green(), person.name);
	Ok(after)
}

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
	for bubble in bubbles {
		session.send(handle, bubble).await?;
		sent_at.push(Timestamp::now());
	}
	Ok(())
}
