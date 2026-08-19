#![doc = include_str!("README.md")]
mod delta;
mod person;
mod sources;

use std::{
	collections::BTreeMap,
	io::Write as _,
	path::{Path, PathBuf},
	process::{Command, Stdio},
	time::Duration,
};

use clap::{Args, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use colored::Colorize as _;
use futures::future::{Either, select};
use grammers_client::Client;
use indicatif::{ProgressBar, ProgressStyle};
use person::Person;
use social_networks_utils::{
	db::Database,
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
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
		RolodexCommand::Open { pattern } => open(&dir, pattern.as_deref()).await,
		RolodexCommand::Pull { pattern } => pull(config, &dir, pattern.as_deref()).await,
	}
}
/// `[rolodex] path` is the directory of person files. No default: a present-but-pathless section is
/// a config mistake, not a request for a guess.
#[derive(Clone, Debug, Default, v_utils::macros::MyConfigPrimitives)]
pub struct RolodexConfig {
	pub path: PathBuf,
}
#[derive(Subcommand)]
enum RolodexCommand {
	/// Open a person file in $EDITOR, creating it when the pattern names nobody yet
	Open { pattern: Option<String> },
	/// Fetch what is new about matching people and fold it into their files
	Pull { pattern: Option<String> },
}

async fn open(dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people = person::load_dir(dir)?;
	let id = match pattern {
		None => {
			if people.is_empty() {
				bail!("no people in {}", dir.display());
			}
			fzf(people.values(), "")?.ok_or_else(|| eyre!("nobody selected"))?
		}
		Some(pattern) => {
			let matches: Vec<&Person> = people.values().filter(|p| p.matches(pattern)).collect();
			match matches.len() {
				0 => {
					eprintln!("no match for `{pattern}`, creating a new person");
					pattern.to_string()
				}
				1 => matches[0].id.clone(),
				_ => fzf(matches.into_iter(), pattern)?.ok_or_else(|| eyre!("nobody selected"))?,
			}
		}
	};

	let person = people.get(&id).cloned().unwrap_or_else(|| Person::skeleton(&id));
	let path = person.path(dir);
	if !path.exists() {
		person.write(dir)?;
	}
	v_utils::io::file_open::open(&path).await.map_err(|e| eyre!("{e:#}"))?;
	person::load_one(&path).wrap_err_with(|| format!("{} no longer evaluates — fix it before anything reads it again", path.display()))?;
	Ok(())
}

async fn pull(config: AppConfig, dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people: Vec<Person> = person::load_dir(dir)?.into_values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	if people.is_empty() {
		bail!("no people in {} matching {}", dir.display(), pattern.unwrap_or("anything"));
	}
	let db = Database::try_new().await?;

	if !people.iter().any(|p| p.handles.contains_key("telegram")) {
		return pull_all(&config, &db, dir, people, None).await;
	}

	// the dialog prefetch inside `connect` takes long enough to look like a hang
	let connecting = spinner("telegram");
	let TelegramConnection { client, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: &config.telegram.username,
		phone: &config.telegram.phone,
		api_id: config.telegram.api_id,
		api_hash: &config.telegram.api_hash,
		session_suffix: "_rolodex",
		seed_from: Some("_dm"),
	})
	.await?;
	connecting.finish_and_clear();
	match select(std::pin::pin!(pull_all(&config, &db, dir, people, Some(&client))), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => bail!("MTProto runner exited during pull"),
	}
}

async fn pull_all(config: &AppConfig, db: &Database, dir: &Path, people: Vec<Person>, telegram: Option<&Client>) -> Result<()> {
	let discord = sources::Discord::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone());
	let github = sources::Github::new();

	let total = people.len();
	let width = people.iter().map(|p| p.id.chars().count()).max().expect("`pull` bails on an empty selection");
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
		let name = format!("{:<width$}", person.id);
		let mut fetched_sources = BTreeMap::new();
		let mut handles = BTreeMap::new();
		let mut messages = Vec::new();
		let mut activity = Vec::new();
		let mut cursors: Vec<(String, String)> = Vec::new();

		for (platform, handle) in &person.handles {
			// the remaining connected-account handles (youtube, battlenet, …) carry no fetch path
			let Some(source) = Source::parse(platform) else { continue };
			pb.set_message(format!("{} {platform}", person.id));
			let cursor = db.rolodex_checkpoint(&person.id, platform).await?;
			let result = match source {
				Source::Discord => discord.fetch(handle, cursor.as_deref()).await,
				Source::Telegram => sources::telegram(telegram.expect("a telegram client is connected iff somebody has a telegram handle"), handle, cursor.as_deref()).await,
				Source::Github => github.fetch(handle, cursor.as_deref()).await,
			};
			match result {
				Ok(fetched) => {
					fetched_sources.extend(fetched.sources);
					handles.extend(fetched.handles);
					messages.extend(fetched.messages);
					activity.extend(fetched.activity);
					if let Some(cursor) = fetched.cursor {
						cursors.push((platform.clone(), cursor));
					}
				}
				// isolated per handle: the checkpoint stays put and the rest of the pull continues
				Err(e) => {
					failures += 1;
					error!("{}: {platform}/{handle} failed, skipping: {e:#}", person.id);
					pb.suspend(|| println!("   {} {name} {platform}/{handle}: {e:#}", "✗".red()));
				}
			}
		}

		let Some(delta) = delta::Delta::new(&person, &fetched_sources, messages, activity) else {
			info!("{}: nothing new", person.id);
			pb.suspend(|| println!("   {} {name} unchanged", "·".dimmed()));
			pb.inc(1);
			continue;
		};
		pb.set_message(format!("{} extracting", person.id));
		let extraction = match delta::extract(&delta).await {
			Ok(extraction) => extraction,
			Err(e) => {
				pb.abandon();
				return Err(e).wrap_err_with(|| format!("extraction for {}", person.id));
			}
		};

		updated += 1;
		entries += extraction.new_log_entries.len();
		info!("{}: +{} log entries over {} sources", person.id, extraction.new_log_entries.len(), fetched_sources.len());
		pb.suspend(|| println!("   {} {name} +{} log entries, {} sources", "✓".green(), extraction.new_log_entries.len(), fetched_sources.len()));
		person.absorb(extraction.summary, extraction.new_log_entries, fetched_sources, handles);
		person.write(dir)?;
		// after the write, so a crash re-fetches rather than losing the messages
		for (platform, cursor) in cursors {
			db.set_rolodex_checkpoint(&person.id, &platform, &cursor).await?;
		}
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

/// The `handles` keys `pull` fetches. Separating this from the dispatch below would let a new source
/// fall through to the no-fetch-path arm and silently do nothing.
enum Source {
	Discord,
	Telegram,
	Github,
}

impl Source {
	fn parse(platform: &str) -> Option<Self> {
		match platform {
			"discord" => Some(Self::Discord),
			"telegram" => Some(Self::Telegram),
			"github" => Some(Self::Github),
			_ => None,
		}
	}
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

/// Returns the chosen file stem. `name` is on the line too, so it is searchable and visible, and a
/// tab keeps it separable from a stem that contains spaces.
fn fzf<'a>(people: impl Iterator<Item = &'a Person>, query: &str) -> Result<Option<String>> {
	let input = people.map(|p| format!("{}\t{}", p.id, p.name.as_deref().unwrap_or_default())).collect::<Vec<_>>().join("\n");
	let mut fzf = Command::new("fzf")
		.args(["--query", query, "--delimiter", "\t", "--with-nth", "1,2"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.wrap_err("failed to run fzf")?;
	fzf.stdin.take().expect("stdin is piped").write_all(input.as_bytes())?;
	let output = fzf.wait_with_output()?;
	if !output.status.success() {
		return Ok(None);
	}
	let chosen = String::from_utf8(output.stdout)?;
	Ok(Some(chosen.trim_end_matches('\n').split('\t').next().expect("split always yields one field").to_string()))
}
