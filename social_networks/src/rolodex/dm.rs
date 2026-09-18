//! The one path that writes to a platform rather than reading from it. Addressed by person, so their
//! directory stays the thing you name and the handle is looked up rather than typed.

use clap::Args;
use color_eyre::eyre::{Result, bail, eyre};
use colored::Colorize as _;
use social_networks_adapters::{
	reach::{Direct, Unreachable},
	skool::Skool,
	telegram_dms, twitter,
};
use social_networks_reach::RolodexConfig;
use strum::AsRefStr;

use super::{person, with_telegram};
use crate::config::AppConfig;

/// Clap has no flag-to-enum, so the group is how [`Messenger`] is spelled on the command line.
#[derive(Args)]
#[group(required = true, multiple = false)]
pub struct MessengerFlag {
	#[arg(long)]
	discord: bool,
	#[arg(long)]
	skool: bool,
	#[arg(long)]
	telegram: bool,
	#[arg(long)]
	twitter: bool,
}
impl From<&MessengerFlag> for Messenger {
	fn from(flag: &MessengerFlag) -> Self {
		match (flag.discord, flag.skool, flag.telegram, flag.twitter) {
			(true, ..) => Self::Discord,
			(_, true, ..) => Self::Skool,
			(_, _, true, _) => Self::Telegram,
			(_, _, _, true) => Self::Twitter,
			_ => unreachable!("clap rejects the command before this when the group is unfilled"),
		}
	}
}

/// `as_ref` is the `handles` key, so a messenger cannot be reachable under a name the person files
/// do not use.
#[derive(AsRefStr, Clone, Copy, Debug)]
#[strum(serialize_all = "lowercase")]
pub enum Messenger {
	Discord,
	Skool,
	Telegram,
	Twitter,
}

/// Exactly one person: `pull` over an ambiguous pattern costs a wasted fetch, a DM over one goes to
/// the wrong human and cannot be taken back.
pub async fn send(config: &AppConfig, rolodex: &RolodexConfig, messenger: Messenger, pattern: &str, text: &str) -> Result<()> {
	let dir = &rolodex.path;
	let people = person::load_dir(dir, &rolodex.tags)?;
	let matches: Vec<&person::Person> = people.values().filter(|p| p.matches(pattern)).collect();
	let [person] = matches[..] else {
		bail!(
			"`{pattern}` matches {} in {}: {}",
			matches.len(),
			dir.display(),
			matches.iter().map(|p| p.name.as_str()).collect::<Vec<_>>().join(", ")
		);
	};
	let platform = messenger.as_ref();
	let handle = person.handles.get(platform).ok_or_else(|| eyre!("{} has no {platform} handle", person.name))?;

	// one `Direct::send`, four sessions: the same enum dispatch the reads go through
	let sent = match messenger {
		Messenger::Discord =>
			social_networks_adapters::discord::Rest::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone())
				.send(handle, text)
				.await,
		// the read path is happy anonymous, but a message is written as somebody
		Messenger::Skool => {
			let credentials = config
				.skool
				.as_ref()
				.ok_or_else(|| eyre!("sending a skool DM signs in, so it needs a `[skool]` section in the config"))?;
			Skool::try_new(Some(credentials.clone()))?.send(handle, text).await
		}
		Messenger::Telegram => with_telegram(&config.telegram, async |client| telegram_dms::Reach { client: &client }.send(handle, text).await).await,
		Messenger::Twitter => twitter::Reach(&config.twitter).send(handle, text).await,
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
	sent?;

	println!("   {} {platform}/{handle} ({})", "✓".green(), person.name);
	Ok(())
}
