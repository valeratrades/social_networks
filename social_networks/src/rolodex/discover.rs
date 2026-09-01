//! From a venue to the people in it. Reads what `recon` already wrote — `members.json` and the
//! transcript — and leaves a skeleton file for everyone the selection names and nobody has yet.
//! `pull` does the rest, because a skeleton with a handle in it is all `pull` has ever needed.
//!
//! Selection is relational: a roster joined against its own line counts. A grammar of our own would
//! be SQL, worse, so the predicate *is* SQL — see [`venue::select`] for the columns. The flags are
//! sugar over the same `WHERE`, so there is one evaluator and one thing to document.

use std::{
	collections::{BTreeMap, BTreeSet},
	path::Path,
};

use clap::Args;
use color_eyre::eyre::{Result, WrapErr};
use colored::Colorize as _;
use jiff::{SignedDuration, Timestamp};
use social_networks_adapters::reach::{Member, VenueRef};
use social_networks_reach::venue::{self, Store};
use v_utils::Timeframe;

use super::person::{self, Person};

#[derive(Args)]
pub struct DiscoverArgs {
	/// `<platform>:<slug>`
	#[arg(value_parser = venue_ref)]
	at: VenueRef,
	/// Only members who have posted since then
	#[arg(long)]
	active_since: Option<Timeframe>,
	#[arg(long)]
	min_posts: Option<usize>,
	/// A sqlite `GLOB` pattern over the handle, e.g. `*-fr`
	#[arg(long)]
	handle_matches: Option<String>,
	/// A SQL `WHERE` clause over the roster table, or a path to a file holding one
	#[arg(long = "where")]
	predicate: Option<String>,
	#[arg(long)]
	limit: Option<usize>,
	/// Write for members who already have a person file too
	#[arg(long)]
	include_known: bool,
	/// Print the selection and write nothing
	#[arg(long)]
	dry_run: bool,
}

pub async fn main(dir: &Path, args: DiscoverArgs) -> Result<()> {
	let store = Store::open(dir, &args.at)?;
	let members = store.roster()?;
	let selected = venue::select(&members, &store.lines(None)?, &where_clause(&args)?).await?;

	let people = person::load_dir(dir)?;
	let platform = args.at.platform.as_ref();
	let known = |member: &Member| people.values().any(|p| p.handles.get(platform) == Some(&member.handle));
	let (already, mut fresh): (Vec<Member>, Vec<Member>) = selected.into_iter().partition(known);
	if args.include_known {
		fresh.extend(already.iter().cloned());
		fresh.sort_by(|a, b| a.handle.cmp(&b.handle));
	}

	let dropped = args.limit.map_or(0, |limit| fresh.len().saturating_sub(limit));
	if let Some(limit) = args.limit {
		fresh.truncate(limit);
	}

	let mut taken: BTreeSet<String> = people.keys().cloned().collect();
	for member in &fresh {
		let stem = stem(member, &mut taken);
		println!(
			"   {} {stem}\t{platform}/{}\t{}",
			if args.dry_run { "?".yellow() } else { "+".green() },
			member.handle,
			member.display
		);
		if args.dry_run {
			continue;
		}
		let mut person = Person::skeleton(&stem);
		person.handles = BTreeMap::from([(platform.to_string(), member.handle.clone())]);
		person.write(dir)?;
	}

	println!(
		"   {} of {}, {} already known{}",
		fresh.len(),
		members.len(),
		already.len(),
		match dropped {
			0 => String::new(),
			n => format!(", {n} past --limit"),
		}
	);
	if !args.dry_run && !fresh.is_empty() {
		println!("   the stems are a guess off the display name — `git mv` any of them, `matches` searches handles too");
	}
	Ok(())
}

/// The flags and `--where`, ANDed. No predicate at all is the whole roster, which is what naming a
/// venue and nothing else asks for.
fn where_clause(args: &DiscoverArgs) -> Result<String> {
	let mut clauses: Vec<String> = Vec::new();
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
		let path = Path::new(predicate);
		// a path or inline SQL, told apart by asking the filesystem — so anything worth an LSP can be
		// written in a `.sql` file
		clauses.push(match path.is_file() {
			true => std::fs::read_to_string(path).wrap_err_with(|| format!("failed to read {}", path.display()))?,
			false => predicate.clone(),
		});
	}
	Ok(match clauses.is_empty() {
		true => "1".to_string(),
		false => clauses.iter().map(|c| format!("({})", c.trim())).collect::<Vec<_>>().join(" AND "),
	})
}

/// `<first>-<last>` off the display name, the handle when there is nothing else, and a numeric suffix
/// when that is taken. The stem is not load-bearing — [`Person::matches`] searches handles too.
fn stem(member: &Member, taken: &mut BTreeSet<String>) -> String {
	let base = match slug(&member.display) {
		Some(slug) => slug,
		None => slug(&member.handle).unwrap_or_else(|| "unnamed".to_string()),
	};
	let mut stem = base.clone();
	//LOOP: bounded by the roster, which is finite and can collide at most once per member
	for n in 2.. {
		if taken.insert(stem.clone()) {
			return stem;
		}
		stem = format!("{base}-{n}");
	}
	unreachable!("the loop returns on the first free stem")
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

fn venue_ref(s: &str) -> std::result::Result<VenueRef, String> {
	s.parse().map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeSet;

	use super::*;

	#[test]
	fn a_stem_is_a_guess_that_never_collides() {
		let member = |display: &str, handle: &str| Member {
			handle: handle.to_string(),
			display: display.to_string(),
			joined: None,
			lat: None,
			lon: None,
			zone: None,
		};
		let mut taken = BTreeSet::from(["lory-bellardant".to_string()]);
		assert_eq!(stem(&member("Lory Bellardant", "lory-bellardant-1253"), &mut taken), "lory-bellardant-2");
		assert_eq!(stem(&member("Lory  Bellardant!", "x"), &mut taken), "lory-bellardant-3");
		assert_eq!(stem(&member("", "josh-lessard-4483"), &mut taken), "josh-lessard-4483");
		assert_eq!(stem(&member("", "🙂"), &mut taken), "unnamed");
	}
}
