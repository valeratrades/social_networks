//! Sends what is due in matching people's outboxes, best ranked first, within each messenger's
//! `per_surface` budget. One message per person per run: the rest of theirs waits for the next.

use std::{collections::BTreeMap, path::Path, str::FromStr as _};

use color_eyre::eyre::{Result, WrapErr as _};
use colored::Colorize as _;
use jiff::Timestamp;
use social_networks_adapters::reach::Unreachable;
use social_networks_reach::{outbox, purpose::Purpose, rank};
use social_networks_utils::db::Database;

use super::{
	dm::{self, Messenger},
	select,
};
use crate::config::AppConfig;

pub async fn main(config: &AppConfig, purpose: &Purpose, venues: &Path, pattern: Option<&str>) -> Result<()> {
	let now = Timestamp::now();
	let db = Database::try_new().await?;
	let mut budget: BTreeMap<Messenger, usize> = BTreeMap::new();
	for ranked in rank::rank(purpose, venues, select(purpose, pattern)?)? {
		let person = &ranked.person;
		let Some(due) = outbox::due(&person.dir(&purpose.path), now)? else { continue };
		let messenger = Messenger::from_str(&due.messenger).wrap_err_with(|| format!("{} is not under a messenger's directory", due.path.display()))?;
		let platform = messenger.as_ref();
		if !budget.contains_key(&messenger) {
			budget.insert(messenger, config.circuit_breakers.remaining(&db, platform).await?);
		}
		let left = budget.get_mut(&messenger).expect("inserted above");
		if *left == 0 {
			println!("   {} {} ({}): {platform} budget spent", "·".dimmed(), person.name, due.at);
			continue;
		}
		if let Some(why) = person.unreachable.get(platform) {
			println!("   {} {} ({}): unreachable on {platform}: {why}", "·".dimmed(), person.name, due.at);
			continue;
		}
		match dm::send_to(config, purpose, person, messenger, &due.text).await {
			Ok(()) => {
				std::fs::remove_file(&due.path).unwrap_or_else(|e| panic!("sent {}, but could not remove it, so the next run would send it again: {e}", due.path.display()));
				*left -= 1;
			}
			Err(e) if e.downcast_ref::<Unreachable>().is_some() => println!("   {} {} ({}): {e}", "✗".red(), person.name, due.at),
			Err(e) => return Err(e),
		}
	}
	for (messenger, left) in &budget {
		println!("   {} {left} left", messenger.as_ref().dimmed());
	}
	Ok(())
}
