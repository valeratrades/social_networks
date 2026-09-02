#![doc = include_str!("README.md")]
mod delta;
mod discover;
mod dm;
mod person;

use std::{
	collections::BTreeMap,
	future::Future,
	io::Write as _,
	path::Path,
	process::{Command, Stdio},
	time::Duration,
};

use clap::{Args, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use colored::Colorize as _;
use grammers_client::Client;
use indicatif::{ProgressBar, ProgressStyle};
use jiff::Timestamp;
use person::Person;
use social_networks_adapters::{
	github::Github,
	linkedin::Linkedin,
	reach::{Author, Direct, Item, Kind, Page, Profiles, Source, VenueRef, Window},
	skool::Skool,
	telegram_dms::{self, TelegramConfig},
};
use social_networks_reach::{
	history::{self, Cursor},
	venue,
};
use tracing::{error, info};

use crate::config::AppConfig;

const TICK: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
#[derive(Args)]
pub struct RolodexArgs {
	#[command(subcommand)]
	command: RolodexCommand,
}

pub async fn main(args: RolodexArgs, config: AppConfig) -> Result<()> {
	let dir = config.rolodex.as_ref().ok_or_else(|| eyre!("no `[rolodex]` section in the config"))?.path.clone();
	match args.command {
		RolodexCommand::Cold { pattern } => cold(&config, &dir, pattern.as_deref()).await,
		RolodexCommand::Discover(args) => discover::main(&dir, args).await,
		RolodexCommand::Dm { messenger, pattern, text } => dm::send(&config, &dir, (&messenger).into(), &pattern, &text).await,
		RolodexCommand::Lines { pattern } => lines(&dir, pattern.as_deref()),
		RolodexCommand::Open { pattern } => open(&dir, pattern.as_deref()).await,
		RolodexCommand::Pull { pattern } => pull(&config, &dir, pattern.as_deref()).await,
	}
}

#[derive(Subcommand)]
enum RolodexCommand {
	/// List matching people no conversation is on record with, on any platform
	Cold { pattern: Option<String> },
	/// Write skeleton files for the members of a venue nobody has a file for yet
	Discover(discover::DiscoverArgs),
	/// Send a message to exactly one matching person over one messenger
	Dm {
		#[command(flatten)]
		messenger: dm::MessengerFlag,
		pattern: String,
		text: String,
	},
	/// Print what matching people said in every venue, straight out of the transcripts
	Lines { pattern: Option<String> },
	/// Open a person file in $EDITOR, creating it when the pattern names nobody yet
	Open { pattern: Option<String> },
	/// Fetch what is new about matching people and fold it into their files
	Pull { pattern: Option<String> },
}

async fn open(dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people = person::load_dir(dir)?;
	let name = match pattern {
		None => {
			if people.is_empty() {
				bail!("no people in {}", dir.display());
			}
			fzf(people.keys(), "")?.ok_or_else(|| eyre!("nobody selected"))?
		}
		Some(pattern) => {
			let matches: Vec<&String> = people.values().filter(|p| p.matches(pattern)).map(|p| &p.name).collect();
			match matches.len() {
				0 => {
					eprintln!("no match for `{pattern}`, creating a new person");
					pattern.to_string()
				}
				1 => matches[0].clone(),
				_ => fzf(matches.into_iter(), pattern)?.ok_or_else(|| eyre!("nobody selected"))?,
			}
		}
	};

	let person = people.get(&name).cloned().unwrap_or_else(|| Person::skeleton(&name));
	let path = person.path(dir);
	if !path.exists() {
		person.write(dir)?;
	}
	v_utils::io::file_open::open(&path).await.map_err(|e| eyre!("{e:#}"))?;
	person::load_one(&path).wrap_err_with(|| format!("{} no longer evaluates — fix it before anything reads it again", path.display()))?;
	Ok(())
}

/// What they said in a venue, in their own words rather than through the labels a pull made of them.
/// This is what outreach is written off, so it prints the whole line and the venue that holds it.
fn lines(dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people = person::load_dir(dir)?;
	let selected: Vec<&Person> = people.values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	if selected.is_empty() {
		bail!("no people in {} matching {}", dir.display(), pattern.unwrap_or("anything"));
	}
	for person in selected {
		let lines = venue_lines(dir, person, None)?;
		let handles: Vec<String> = person.handles.iter().map(|(platform, handle)| format!("{platform}/{handle}")).collect();
		println!("\n{} {}", person.name.bold(), handles.join(" ").dimmed());
		if lines.is_empty() {
			println!("   {} nothing in any venue transcript on disk", "·".dimmed());
		}
		for (at, line) in lines {
			println!("   {} {}  {}", line.at.to_string().dimmed(), at.to_string().dimmed(), line.text.replace('\n', "\n     "));
		}
	}
	Ok(())
}

/// Everybody no conversation is on record with, on any platform that could hold one. What a person
/// said in a venue is not a conversation with them — it stayed in the venue's transcript, and is why
/// the members `discover` wrote a file for come out cold until somebody writes to them.
async fn cold(config: &AppConfig, dir: &Path, pattern: Option<&str>) -> Result<()> {
	let selected: Vec<Person> = person::load_dir(dir)?.into_values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	let total = selected.len();
	let candidates = sift(dir, selected)?;

	let telegram = candidates.iter().any(|(_, ask)| ask.contains(&Source::Telegram));
	let cold = match telegram {
		true => with_telegram(&config.telegram, async |client| probe_all(config, dir, candidates, Some(&client)).await).await?,
		false => probe_all(config, dir, candidates, None).await?,
	};

	let width = cold.iter().map(|p| p.name.chars().count()).max().unwrap_or(0);
	for person in &cold {
		let handles: Vec<String> = person.handles.iter().map(|(platform, handle)| format!("{platform}/{handle}")).collect();
		println!("   {} {:<width$} {}", "·".dimmed(), person.name, handles.join(" ").dimmed());
	}
	println!("   {} of {total} cold", cold.len());
	Ok(())
}

/// Everybody the record does not already place a conversation with, and per person the sources it
/// says nothing either way about — those are what [`probe_all`] then asks.
fn sift(dir: &Path, people: Vec<Person>) -> Result<Vec<(Person, Vec<Source>)>> {
	let mut candidates = Vec::new();
	for person in people {
		let meta = history::Meta::load(&person.dir(dir))?;
		let sources: Vec<Source> = person.handles.keys().filter_map(|platform| platform.parse::<Source>().ok()).collect();
		if sources.iter().any(|source| meta.messages(*source).is_some_and(|messages| messages > 0)) {
			continue;
		}
		let ask = sources.into_iter().filter(|source| source.has_history() && meta.messages(*source).is_none()).collect();
		candidates.push((person, ask));
	}
	Ok(candidates)
}

/// One message per source, which is all it takes to answer whether there is a conversation. Nothing
/// is written: what the messages *say* is `pull`'s, and a probe that turned into an archive would
/// leave a transcript no backfill may finish.
async fn probe_all(config: &AppConfig, dir: &Path, candidates: Vec<(Person, Vec<Source>)>, telegram: Option<&Client>) -> Result<Vec<Person>> {
	let mut discord = social_networks_adapters::discord::Rest::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone());
	let mut skool = Skool::try_new(config.skool.clone())?;
	let mut cold = Vec::new();
	for (person, ask) in candidates {
		let assets = person.dir(dir).join("assets");
		// a source that cannot answer is not an answer either: it leaves them off the list rather
		// than on it
		let mut exclude = false;
		for source in ask {
			let handle = &person.handles[source.as_ref()];
			let page = match source {
				Source::Discord => discord.direct(handle, Window::probe(), &assets).await,
				Source::Telegram => {
					let mut client = telegram_dms::Reach {
						client: telegram.expect("a telegram client is connected iff a telegram handle is being asked"),
					};
					client.direct(handle, Window::probe(), &assets).await
				}
				Source::Skool => skool.direct(handle, Window::probe(), &assets).await,
				// `has_history` is what put a source in the list
				Source::Github | Source::Linkedin => unreachable!("{} holds no conversation", source.as_ref()),
			};
			match page {
				Ok(page) => exclude |= !page.items.is_empty(),
				Err(e) => {
					exclude = true;
					println!("   {} {} {}/{handle}: {e:#}", "✗".red(), person.name, source.as_ref());
				}
			}
		}
		if !exclude {
			cold.push(person);
		}
	}
	Ok(cold)
}

async fn pull(config: &AppConfig, dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people: Vec<Person> = person::load_dir(dir)?.into_values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	if people.is_empty() {
		bail!("no people in {} matching {}", dir.display(), pattern.unwrap_or("anything"));
	}

	// A pattern says who; without one this is the whole rolodex, and whoever has no cursor yet is
	// read from their first message rather than from where the last read stopped.
	if pattern.is_none() {
		let mut whole: Vec<&str> = Vec::new();
		for person in &people {
			let meta = history::Meta::load(&person.dir(dir))?;
			let sources = person.handles.keys().filter_map(|platform| platform.parse::<Source>().ok());
			let unread = sources.filter(|source| source.has_history()).any(|source| meta.messages(source).is_none());
			if unread || meta.backfill_status().is_some() {
				whole.push(&person.name);
			}
		}
		let scope = match whole.is_empty() {
			true => format!("pull {} people, each from where the last read stopped", people.len()),
			false => format!("pull {} people, {} of them in full ({})", people.len(), whole.len(), whole.join(", ")),
		};
		if v_utils::io::confirmation(&scope).flush_blocking() == v_utils::io::ConfirmResult::No {
			return Ok(());
		}
	}

	if !people.iter().any(|p| p.handles.contains_key("telegram")) {
		return pull_all(config, dir, people, None).await;
	}
	with_telegram(&config.telegram, async |client| pull_all(config, dir, people, Some(&client)).await).await
}

/// The dialog prefetch inside `connect` takes long enough to look like a hang without the spinner.
pub(super) async fn with_telegram<T, F: Future<Output = Result<T>>>(config: &TelegramConfig, f: impl FnOnce(Client) -> F) -> Result<T> {
	let connecting = spinner("telegram");
	social_networks_reach::with_telegram(config, |client| {
		connecting.finish_and_clear();
		f(client)
	})
	.await
}

/// What one platform had to say about one person this run.
#[derive(Default)]
struct Fetched {
	sources: BTreeMap<String, String>,
	handles: BTreeMap<String, String>,
	items: Vec<Item>,
}

async fn pull_all(config: &AppConfig, dir: &Path, people: Vec<Person>, telegram: Option<&Client>) -> Result<()> {
	let llm_config = config.require_llm("rolodex")?;
	let mut discord = social_networks_adapters::discord::Rest::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone());
	let mut github = Github::default();
	let mut linkedin = Linkedin;
	// credentials only widen what skool answers; without them the public profile is still a whole result
	let mut skool = Skool::try_new(config.skool.clone())?;

	let total = people.len();
	let width = people.iter().map(|p| p.name.chars().count()).max().expect("`pull` bails on an empty selection");
	let pb = ProgressBar::new(total as u64);
	pb.set_style(
		ProgressStyle::with_template(" {spinner:.cyan} {prefix:.bold} [{elapsed_precise}] {bar:30.cyan/238} {pos:>2}/{len} {msg:.dim}")
			.expect("static template")
			.tick_strings(TICK),
	);
	pb.set_prefix("rolodex");
	pb.enable_steady_tick(Duration::from_millis(80));
	let (mut updated, mut entries, mut failures) = (0usize, 0usize, 0usize);

	for mut person in people {
		let name = format!("{:<width$}", person.name);
		let person_dir = person.dir(dir);
		let assets = person_dir.join("assets");
		let mut meta = history::Meta::load(&person_dir)?;
		let mut fetched_sources = BTreeMap::new();
		let mut handles = BTreeMap::new();
		let mut fetched = Vec::new();

		for (platform, handle) in &person.handles {
			// the remaining connected-account handles (youtube, battlenet, …) carry no fetch path
			let Ok(source) = platform.parse::<Source>() else { continue };
			pb.set_message(format!("{} {platform}", person.name));
			let mut cursor = meta.cursor(source)?;
			// exhaustive: a source that grows a fetch path is handled here or nothing compiles
			let result = match source {
				Source::Discord => converse(&mut discord, handle, &mut cursor, &assets).await,
				Source::Telegram => {
					let mut client = telegram_dms::Reach {
						client: telegram.expect("a telegram client is connected iff somebody has a telegram handle"),
					};
					converse(&mut client, handle, &mut cursor, &assets).await
				}
				Source::Github => stated(&mut github, handle, &mut cursor).await,
				Source::Linkedin => stated(&mut linkedin, handle, &mut cursor).await,
				Source::Skool => converse(&mut skool, handle, &mut cursor, &assets).await,
			};
			match result {
				Ok(fetch) => {
					fetched_sources.extend(fetch.sources);
					handles.extend(fetch.handles);
					fetched.extend(fetch.items);
				}
				// isolated per handle: whatever the backfill already checked in stands, and the rest of
				// the pull continues
				Err(e) => {
					failures += 1;
					error!("{}: {platform}/{handle} failed, skipping: {e:#}", person.name);
					pb.suspend(|| println!("   {} {name} {platform}/{handle}: {e:#}", "✗".red()));
				}
			}
		}

		// before the extraction, not after it: the transcript is the durable record now and the labels
		// are derived from it, so a failed LLM call costs a re-run rather than the messages. A person's
		// year files hold their DMs; what they said in a venue stays in the venue's.
		let direct: Vec<Item> = fetched.iter().filter(|item| item.kind == Kind::Direct).cloned().collect();
		history::record(&person_dir, direct, &mut meta)?;
		let state = meta.backfill_status().map(|s| format!(", {s}")).unwrap_or_default();

		// costs no network: `recon posts` already paid for these
		let from_venues = venue_items(dir, &person, meta.venues_through())?;
		let through = from_venues.iter().map(|item| item.at).max();
		fetched.extend(from_venues);

		let Some(delta) = delta::Delta::new(&person, &fetched_sources, fetched) else {
			info!("{}: nothing new", person.name);
			pb.suspend(|| println!("   {} {name} unchanged{state}", "·".dimmed()));
			pb.inc(1);
			continue;
		};
		pb.set_message(format!("{} extracting", person.name));
		let extraction = match delta::extract(&delta, &llm_config).await {
			Ok(extraction) => extraction,
			Err(e) => {
				pb.abandon();
				return Err(e).wrap_err_with(|| format!("extraction for {}", person.name));
			}
		};

		pb.set_message(format!("{} discovering handles", person.name));
		let discovered = match delta::discover_handles(&delta, &llm_config).await {
			Ok(discovered) => discovered,
			Err(e) => {
				pb.abandon();
				return Err(e).wrap_err_with(|| format!("handle discovery for {}", person.name));
			}
		};
		// what a platform reports about itself outranks what an LLM read out of a conversation
		let mut added: Vec<String> = Vec::new();
		for (platform, handle) in discovered {
			if !person.handles.contains_key(&platform) && !handles.contains_key(&platform) {
				added.push(platform.clone());
				handles.insert(platform, handle);
			}
		}

		// only once the extraction has actually read them, so a failed call costs a re-read
		meta.venues_read(through)?;
		updated += 1;
		entries += extraction.new_log_entries.len();
		info!("{}: +{} log entries over {} sources", person.name, extraction.new_log_entries.len(), fetched_sources.len());
		let added = if added.is_empty() { String::new() } else { format!(", +{}", added.join(" +")) };
		pb.suspend(|| {
			println!(
				"   {} {name} +{} log entries, {} sources{added}{state}",
				"✓".green(),
				extraction.new_log_entries.len(),
				fetched_sources.len()
			)
		});
		person.absorb(extraction.summary, extraction.new_log_entries, fetched_sources, handles);
		person.write(dir)?;
		pb.inc(1);
	}

	let failures = if failures == 0 { String::new() } else { format!(", {failures} failed") };
	let summary = format!("{total} {}, {updated} updated, +{entries} log entries{failures}", if total == 1 { "person" } else { "people" });
	// the finished bar is the summary wherever it renders; without a tty it never does
	if pb.is_hidden() {
		println!("   {summary}");
	}
	pb.set_style(ProgressStyle::with_template(" ✓ {prefix:.bold.green} [{elapsed_precise}] {bar:30.green/238} {pos:>2}/{len} {msg:.green}").expect("static template"));
	pb.finish_with_message(summary);
	Ok(())
}

/// What a platform states, for the sources that hold no conversation.
async fn stated<C: Profiles>(client: &mut C, handle: &str, cursor: &mut Cursor<'_>) -> Result<Fetched> {
	let profile = client.profile(handle, Window::above(cursor.newest().map(str::to_string))).await?;
	if let Some(newest) = &profile.activity.newest {
		cursor.advance(newest.clone());
	}
	Ok(Fetched {
		sources: profile.sources,
		handles: profile.handles,
		items: profile.activity.items,
	})
}

/// What a messenger states, plus the conversation itself and whatever backfill is still owed.
async fn converse<C: Profiles + Direct>(client: &mut C, handle: &str, cursor: &mut Cursor<'_>, assets: &Path) -> Result<Fetched> {
	let profile = client.profile(handle, Window::above(cursor.newest().map(str::to_string))).await?;
	let page = client.direct(handle, Window::above(cursor.newest().map(str::to_string)), assets).await?;
	if let Some(newest) = &page.newest {
		cursor.advance(newest.clone());
	}

	// before the backfill, not after it: a page checked in below this slice persists the cursor above
	// it, and a kill in between would leave the slice in no file at all
	if cursor.archiving() {
		cursor.stash(&page.items)?;
	}
	if cursor.backfilling() {
		match page.exhausted && cursor.floor().is_none() {
			// the first read already reached the first message of the conversation
			true => cursor.exhausted()?,
			false => backfill(client, handle, cursor, assets, page.oldest.clone()).await?,
		}
	}
	Ok(Fetched {
		sources: profile.sources,
		handles: profile.handles,
		items: page.items,
	})
}

/// Down to the first message of the conversation, checking every page in before asking for the next
/// — so an interrupt costs the page in flight and nothing behind it.
async fn backfill<C: Direct>(client: &mut C, handle: &str, cursor: &mut Cursor<'_>, assets: &Path, incremental_floor: Option<String>) -> Result<()> {
	// the slice fetched above is already accounted for, so the walk starts under it
	let mut before = cursor.floor().map(str::to_string).or(incremental_floor);
	// the short page that ends the walk cannot be predicted from the page before it
	//LOOP: bounded by the conversation, which is finite and walked strictly downwards
	loop {
		let page: Page = client.direct(handle, Window::below(before.clone()), assets).await?;
		let Some(oldest) = page.oldest.clone() else { break };
		cursor.page(&page.items, oldest.clone())?;
		if page.exhausted {
			break;
		}
		before = Some(oldest);
	}
	cursor.exhausted()
}

/// This person's own lines out of every venue transcript on disk, and which venue each came from.
/// Nothing is fetched: `recon posts` already paid for them, and only the lines the slot attributes
/// to a handle of theirs are read.
fn venue_lines(dir: &Path, person: &Person, since: Option<Timestamp>) -> Result<Vec<(VenueRef, venue::Line)>> {
	let mut out = Vec::new();
	for at in venue::all(dir)? {
		let Some(handle) = person.handles.get(at.platform.as_ref()) else { continue };
		let store = venue::Store::open(dir, &at)?;
		out.extend(store.lines(since)?.into_iter().filter(|line| &line.handle == handle).map(|line| (at.clone(), line)));
	}
	out.sort_by_key(|(_, line)| line.at);
	Ok(out)
}

fn venue_items(dir: &Path, person: &Person, since: Option<Timestamp>) -> Result<Vec<Item>> {
	Ok(venue_lines(dir, person, since)?
		.into_iter()
		.map(|(at, line)| Item {
			id: format!("{at}:{}", line.at),
			source: at.platform.into(),
			at: line.at,
			kind: Kind::Post,
			author: Author::Handle(line.handle),
			text: line.text,
			attachments: Vec::new(),
			permalink: None,
		})
		.collect())
}

fn spinner(prefix: &'static str) -> ProgressBar {
	let pb = ProgressBar::new_spinner();
	pb.set_style(
		ProgressStyle::with_template(" {spinner:.cyan} {prefix:.bold} [{elapsed_precise}] {msg:.dim}")
			.expect("static template")
			.tick_strings(TICK),
	);
	pb.set_prefix(prefix);
	pb.set_message("connecting");
	pb.enable_steady_tick(Duration::from_millis(80));
	pb
}

fn fzf<'a>(names: impl Iterator<Item = &'a String>, query: &str) -> Result<Option<String>> {
	let input = names.cloned().collect::<Vec<_>>().join("\n");
	let mut fzf = Command::new("fzf")
		.args(["--query", query])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.wrap_err("failed to run fzf")?;
	fzf.stdin.take().expect("stdin is piped").write_all(input.as_bytes())?;
	let output = fzf.wait_with_output()?;
	if !output.status.success() {
		return Ok(None);
	}
	Ok(Some(String::from_utf8(output.stdout)?.trim().to_string()))
}

#[cfg(test)]
mod tests {
	use std::collections::BTreeMap;

	use super::*;

	/// The record answers for whatever it has been kept for, and a source it has never been kept for
	/// is what costs a request — never the other way round.
	#[test]
	fn only_a_source_the_record_cannot_answer_for_is_asked() {
		let dir = std::env::temp_dir().join("social_networks_rolodex_cold");
		let _ = std::fs::remove_dir_all(&dir);
		let meta = |name: &str, json: &str| {
			let person_dir = Person::skeleton(name).dir(&dir);
			std::fs::create_dir_all(&person_dir).unwrap();
			std::fs::write(person_dir.join("meta.json"), json).unwrap();
		};
		let person = |name: &str, handles: &[(&str, &str)]| {
			let mut person = Person::skeleton(name);
			person.handles = handles.iter().map(|(p, h)| (p.to_string(), h.to_string())).collect();
			person
		};
		let source = |messages: usize| format!(r#"{{"newest":null,"oldest":null,"backfill_done":true,"messages":{messages},"last_message":null}}"#);

		meta("asked", &format!(r#"{{"sources":{{"skool":{}}}}}"#, source(0)));
		meta("silent", &format!(r#"{{"sources":{{"discord":{}}}}}"#, source(0)));
		meta("spoke", &format!(r#"{{"sources":{{"discord":{}}}}}"#, source(3)));
		meta("half", &format!(r#"{{"sources":{{"skool":{}}}}}"#, source(0)));

		let people = vec![
			person("asked", &[("skool", "asked-1")]),
			person("silent", &[("discord", "silent")]),
			person("spoke", &[("discord", "spoke")]),
			// skool answered, telegram never did — the conversation could still be there
			person("half", &[("skool", "half-2"), ("telegram", "half")]),
			person("never", &[("telegram", "never")]),
		];
		let candidates = sift(&dir, people).unwrap();
		let asked: Vec<(&str, Vec<&str>)> = candidates.iter().map(|(p, ask)| (p.name.as_str(), ask.iter().map(|s| s.as_ref()).collect())).collect();
		assert_eq!(
			asked,
			[
				("asked", vec![]),
				("silent", vec![]),
				// `spoke` is not here at all: the record already places a conversation
				("half", vec!["telegram"]),
				("never", vec!["telegram"]),
			]
		);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// What the venue axis is for, and the one thing it must never get wrong: a pull folds in this
	/// person's lines and nobody else's, out of a transcript that keeps the whole conversation.
	#[test]
	fn a_pull_takes_only_its_own_lines_out_of_a_venue() {
		let dir = std::env::temp_dir().join("social_networks_rolodex_venue_pull");
		let _ = std::fs::remove_dir_all(&dir);
		let venue = dir.join("venues").join("skool").join("20kmodrop");
		std::fs::create_dir_all(&venue).unwrap();
		std::fs::write(venue.join("meta.json"), "{}").unwrap();
		std::fs::write(
			venue.join("2026.md"),
			"# skool:20kmodrop — 2026 (times UTC)\n\
			 \n## 2026-03-04\n\n\
			 - 10:00:00 [lory/skool@20kmodrop] shipped the thing\n  \
			 and it works\n\
			 - 11:00:00 [josh/skool@20kmodrop] congrats\n\
			 \n## 2026-03-05\n\n\
			 - 09:00:00 [lory/skool@20kmodrop] next one\n",
		)
		.unwrap();

		let mut lory = Person::skeleton("lory-bellardant");
		lory.handles = BTreeMap::from([("skool".to_string(), "lory".to_string())]);
		let items = venue_items(&dir, &lory, None).unwrap();
		assert_eq!(items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["shipped the thing\nand it works", "next one"]);
		assert!(items.iter().all(|i| i.kind == Kind::Post && i.source == Source::Skool));

		// the same file, read for somebody it does not name at all
		let mut stranger = Person::skeleton("stranger");
		stranger.handles = BTreeMap::from([("skool".to_string(), "nobody".to_string())]);
		assert!(venue_items(&dir, &stranger, None).unwrap().is_empty());

		// and what the extraction has already seen does not come back
		let through = items.iter().map(|i| i.at).max();
		assert_eq!(venue_items(&dir, &lory, through).unwrap().len(), 1, "`since` is inclusive of its own floor");
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
