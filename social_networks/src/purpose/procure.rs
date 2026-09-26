//! Into a purpose, from what `recon` already wrote. A strategy selects over a venue's roster joined
//! against its transcript: whoever it names that the purpose lacks gets a skeleton, and everyone it
//! names gets its tags. `pull` does the rest, because a skeleton with a handle in it is all `pull` has
//! ever needed. Nothing here fetches, so `recon` stays the only thing that spends a request.
//!
//! Selection is relational: a roster joined against its own line counts. A grammar of our own would
//! be SQL, worse, so the predicate *is* SQL — see [`venue::select`] for the columns. The flags are
//! sugar over the same `WHERE`, so there is one evaluator and one thing to document.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::Path,
};

use clap::Args;
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use colored::Colorize as _;
use jiff::{SignedDuration, Timestamp};
use social_networks_adapters::reach::Member;
use social_networks_reach::{
	person::{self, Person},
	purpose::{Purpose, Strategy},
	venue::{self, Store},
};
use v_utils::Timeframe;

#[derive(Args)]
pub struct ProcureArgs {
	/// A strategy of this purpose, or `<platform>:<slug>` for a venue ad hoc. Every strategy when omitted
	target: Option<String>,
	/// Only members who have posted since then
	#[arg(long)]
	active_since: Option<Timeframe>,
	#[arg(long)]
	min_posts: Option<usize>,
	/// A sqlite `GLOB` pattern over the handle, e.g. `*-fr`
	#[arg(long)]
	handle_matches: Option<String>,
	/// A SQL `WHERE` clause over the roster table, or a path to a file holding one. ANDed with the
	/// strategy's own
	#[arg(long = "where")]
	predicate: Option<String>,
	/// At most this many new people per strategy
	#[arg(long)]
	limit: Option<usize>,
	/// Print the selection and write nothing
	#[arg(long)]
	dry_run: bool,
}

pub async fn main(purpose: &Purpose, venues: &Path, args: ProcureArgs) -> Result<()> {
	let adhoc: Strategy;
	let strategies: Vec<(&str, &Strategy)> = match args.target.as_deref() {
		None => {
			if purpose.procure.is_empty() {
				bail!("`purposes.{}.procure` names no strategy; `procure <platform>:<slug>` runs a venue ad hoc", purpose.name);
			}
			purpose.procure.iter().map(|(name, strategy)| (name.as_str(), strategy)).collect()
		}
		// strategy names are refused a colon at load, so this cannot shadow one
		Some(target) if target.contains(':') => {
			adhoc = Strategy::Venue {
				at: target.parse()?,
				predicate: None,
				tags: BTreeMap::new(),
			};
			vec![(target, &adhoc)]
		}
		Some(name) => vec![(
			name,
			purpose.procure.get(name).ok_or_else(|| {
				eyre!(
					"`{name}` is neither `<platform>:<slug>` nor a strategy of `purposes.{}.procure`: {}",
					purpose.name,
					purpose.procure.keys().cloned().collect::<Vec<_>>().join(", ")
				)
			})?,
		)],
	};

	// one set of people across every strategy, so a person the first creates is known to the second —
	// on a dry run too, which is what keeps it a preview of the real one
	let mut people = person::load_dir(purpose)?;
	let mut created = 0;
	for (name, strategy) in strategies {
		println!("{}", name.bold());
		created += run(purpose, venues, &mut people, strategy, &args).await?;
	}
	if !args.dry_run && created > 0 {
		println!("   the names are a guess off the display name — `git mv` any of them, `matches` searches handles too");
	}
	Ok(())
}

/// Existing people are never re-created; the strategy's tags go on everyone it selects. Returns how
/// many it created.
async fn run(purpose: &Purpose, venues: &Path, people: &mut BTreeMap<String, Person>, strategy: &Strategy, args: &ProcureArgs) -> Result<usize> {
	let Strategy::Venue { at, predicate, tags } = strategy;
	let store = Store::open(venues, at)?;
	let members = store.roster()?;
	let selected = venue::select(&members, &store.lines(None)?, &where_clause(predicate.as_deref(), args)?).await?;
	let platform = at.platform.as_ref();
	let dir = &purpose.path;
	let (fresh_mark, retag_mark) = match args.dry_run {
		true => ("?".yellow(), "~".yellow()),
		false => ("+".green(), "~".green()),
	};

	let (mut already, mut retagged, mut fresh) = (0usize, 0usize, Vec::new());
	for member in selected {
		let Some(name) = people.values().find(|p| p.handles.get(platform) == Some(&member.handle)).map(|p| p.name.clone()) else {
			fresh.push(member);
			continue;
		};
		already += 1;
		let person = people.get_mut(&name).expect("found among these very people");
		let before = person.tags.clone();
		person.tags.extend(tags.clone());
		if person.tags != before {
			retagged += 1;
			println!("   {retag_mark} {name}\t{platform}/{}", member.handle);
			if !args.dry_run {
				person.write(dir)?;
			}
		}
	}

	let dropped = args.limit.map_or(0, |limit| fresh.len().saturating_sub(limit));
	if let Some(limit) = args.limit {
		fresh.truncate(limit);
	}
	let mut taken: BTreeSet<String> = people.keys().cloned().collect();
	for member in &fresh {
		let name = name(member, &mut taken);
		println!("   {fresh_mark} {name}\t{platform}/{}\t{}", member.handle, member.display);
		let mut person = Person::skeleton(&name);
		person.handles = BTreeMap::from([(platform.to_string(), member.handle.clone())]);
		person.tags = tags.clone();
		if !args.dry_run {
			person.write(dir)?;
		}
		people.insert(name, person);
	}

	println!(
		"   {} of {}, {already} already known{}{}",
		fresh.len(),
		members.len(),
		match retagged {
			0 => String::new(),
			n => format!(", {n} retagged"),
		},
		match dropped {
			0 => String::new(),
			n => format!(", {n} past --limit"),
		}
	);
	Ok(fresh.len())
}

/// The strategy's own predicate, the flags and `--where`, ANDed. No predicate at all is the whole
/// roster, which is what naming a venue and nothing else asks for.
fn where_clause(own: Option<&str>, args: &ProcureArgs) -> Result<String> {
	let mut clauses: Vec<String> = Vec::new();
	if let Some(own) = own {
		clauses.push(venue::clause(own)?);
	}
	if let Some(since) = &args.active_since {
		let floor = Timestamp::now() - SignedDuration::try_from(since.duration()).wrap_err("an --active-since is milliseconds")?;
		clauses.push(format!("last_post >= '{floor}'"));
	}
	if let Some(min) = args.min_posts {
		clauses.push(format!("posts >= {min}"));
	}
	if let Some(glob) = &args.handle_matches {
		clauses.push(format!("handle GLOB '{}'", glob.replace('\'', "''")));
	}
	if let Some(predicate) = &args.predicate {
		clauses.push(venue::clause(predicate)?);
	}
	Ok(match clauses.is_empty() {
		true => "1".to_string(),
		false => clauses.iter().map(|c| format!("({})", c.trim())).collect::<Vec<_>>().join(" AND "),
	})
}

/// `<first>-<last>` off the display name, the handle when there is nothing else, and a numeric suffix
/// when that is taken. The name is not load-bearing — [`Person::matches`] searches handles too.
fn name(member: &Member, taken: &mut BTreeSet<String>) -> String {
	let base = match slug(&member.display) {
		Some(slug) => slug,
		None => slug(&member.handle).unwrap_or_else(|| "unnamed".to_string()),
	};
	let mut name = base.clone();
	//LOOP: bounded by the roster, which is finite and can collide at most once per member
	for n in 2.. {
		if taken.insert(name.clone()) {
			return name;
		}
		name = format!("{base}-{n}");
	}
	unreachable!("the loop returns on the first free name")
}

fn slug(name: &str) -> Option<String> {
	let slug: String = name
		.trim()
		.to_lowercase()
		.chars()
		.map(|c| if c.is_alphanumeric() { c } else { '-' })
		.collect::<String>()
		.split('-')
		.filter(|part| !part.is_empty())
		.collect::<Vec<_>>()
		.join("-");
	(!slug.is_empty()).then_some(slug)
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::*;

	#[test]
	fn a_name_is_a_guess_that_never_collides() {
		let member = |display: &str, handle: &str| Member {
			handle: handle.to_string(),
			display: display.to_string(),
			joined: None,
			lat: None,
			lon: None,
			zone: None,
		};
		let mut taken = BTreeSet::from(["lory-bellardant".to_string()]);
		assert_eq!(name(&member("Lory Bellardant", "lory-bellardant-1253"), &mut taken), "lory-bellardant-2");
		assert_eq!(name(&member("Lory  Bellardant!", "x"), &mut taken), "lory-bellardant-3");
		assert_eq!(name(&member("", "josh-lessard-4483"), &mut taken), "josh-lessard-4483");
		assert_eq!(name(&member("", "🙂"), &mut taken), "unnamed");
	}
}
