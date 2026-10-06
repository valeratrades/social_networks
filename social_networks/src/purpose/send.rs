//! Sends what is due in the outboxes of exactly `n` matching people, best ranked first. One message
//! per person per run: the rest of theirs waits for the next.

use std::{path::Path, str::FromStr as _};

use color_eyre::eyre::{Result, WrapErr as _, bail};
use colored::Colorize as _;
use jiff::Timestamp;
use social_networks_adapters::reach::Unreachable;
use social_networks_reach::{outbox, purpose::Purpose, rank};

use super::{
	dm::{self, Messenger},
	select,
};
use crate::config::AppConfig;

pub async fn main(config: &AppConfig, purpose: &Purpose, venues: &Path, pattern: Option<&str>, n: usize, multi_message: bool, noise: Option<&dm::Noise>) -> Result<()> {
	let now = Timestamp::now();
	let mut sendable = Vec::new();
	for ranked in rank::rank(purpose, venues, select(purpose, pattern)?)? {
		let Some(due) = outbox::due(&ranked.person.dir(&purpose.path), now)? else { continue };
		let messenger = Messenger::from_str(&due.messenger).wrap_err_with(|| format!("{} is not under a messenger's directory", due.path.display()))?;
		if let Some(why) = ranked.person.unreachable.get(messenger.as_ref()) {
			println!("   {} {} ({}): unreachable on {}: {why}", "·".dimmed(), ranked.person.name, due.at, messenger.as_ref());
			continue;
		}
		sendable.push((ranked.person, messenger, due));
	}
	if sendable.len() < n {
		bail!(
			"asked to send to {n}, and only {} matching people have something due and are not known unreachable",
			sendable.len()
		);
	}

	let mut sent = 0;
	for (person, messenger, due) in &sendable {
		if sent == n {
			break;
		}
		match dm::send_to(config, purpose, person, *messenger, &due.text, multi_message, noise).await {
			Ok(after) => {
				std::fs::remove_file(&due.path).unwrap_or_else(|e| panic!("sent {}, but could not remove it, so the next run would send it again: {e}", due.path.display()));
				sent += 1;
				after.wrap_err_with(|| format!("after the message to {} went out", person.name))?;
			}
			Err(e) => {
				if let Some(partly) = e.downcast_ref::<dm::PartlySent>() {
					std::fs::write(&due.path, &partly.rest).unwrap_or_else(|w| {
						panic!(
							"{} went out, but {} could not be cut down to the rest, so the next run would send them again: {w}",
							partly.sent,
							due.path.display()
						)
					});
				}
				match e.downcast_ref::<Unreachable>() {
					Some(_) => println!("   {} {} ({}): {e:#}", "✗".red(), person.name, due.at),
					None => return Err(e),
				}
			}
		}
	}
	if sent < n {
		bail!("sent {sent} of {n}: the rest of the {} sendable turned out unreachable", sendable.len());
	}
	Ok(())
}
