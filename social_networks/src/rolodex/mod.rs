#![doc = include_str!("README.md")]
mod delta;
mod dm;
mod person;
mod sources;

use std::{
	collections::BTreeMap,
	future::Future,
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
use social_networks_adapters::telegram_dms::TelegramConfig;
use social_networks_utils::{
	db::Database,
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
};
use sources::Source;
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
		RolodexCommand::Dm { messenger, pattern, text } => dm::send(&config, &dir, (&messenger).into(), &pattern, &text).await,
		RolodexCommand::Open { pattern } => open(&dir, pattern.as_deref()).await,
		RolodexCommand::Pull { pattern } => pull(&config, &dir, pattern.as_deref()).await,
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
	/// Send a message to exactly one matching person over one messenger
	Dm {
		#[command(flatten)]
		messenger: dm::MessengerFlag,
		pattern: String,
		text: String,
	},
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

async fn pull(config: &AppConfig, dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people: Vec<Person> = person::load_dir(dir)?.into_values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	if people.is_empty() {
		bail!("no people in {} matching {}", dir.display(), pattern.unwrap_or("anything"));
	}
	let db = Database::try_new().await?;

	if !people.iter().any(|p| p.handles.contains_key("telegram")) {
		return pull_all(config, &db, dir, people, None).await;
	}
	with_telegram(&config.telegram, async |client| pull_all(config, &db, dir, people, Some(&client)).await).await
}

/// The runner has to be polled alongside whatever uses the client, and the dialog prefetch inside
/// `connect` takes long enough to look like a hang without the spinner.
async fn with_telegram<T, F: Future<Output = Result<T>>>(config: &TelegramConfig, f: impl FnOnce(Client) -> F) -> Result<T> {
	let connecting = spinner("telegram");
	let TelegramConnection { client, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: &config.username,
		phone: &config.phone,
		api_id: config.api_id,
		api_hash: &config.api_hash,
		session_suffix: "_rolodex",
		seed_from: Some("_dm"),
	})
	.await?;
	connecting.finish_and_clear();
	match select(std::pin::pin!(f(client)), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => Err(eyre!("MTProto runner exited mid-call")),
	}
}

async fn pull_all(config: &AppConfig, db: &Database, dir: &Path, people: Vec<Person>, telegram: Option<&Client>) -> Result<()> {
	let llm_config = config.require_llm("rolodex")?;
	let discord = sources::Discord::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone());
	let github = sources::Github::default();

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
		let mut fetched_sources = BTreeMap::new();
		let mut handles = BTreeMap::new();
		let mut messages = Vec::new();
		let mut activity = Vec::new();
		let mut cursors: Vec<(String, String)> = Vec::new();

		for (platform, handle) in &person.handles {
			// the remaining connected-account handles (youtube, battlenet, …) carry no fetch path
			let Ok(source) = platform.parse::<Source>() else { continue };
			pb.set_message(format!("{} {platform}", person.name));
			let cursor = db.rolodex_checkpoint(&person.name, platform).await?;
			let result = match source {
				Source::Discord => discord.fetch(handle, cursor.as_deref()).await,
				Source::Telegram => sources::telegram(telegram.expect("a telegram client is connected iff somebody has a telegram handle"), handle, cursor.as_deref()).await,
				Source::Github => github.fetch(handle, cursor.as_deref()).await,
				Source::Linkedin => sources::linkedin(handle, cursor.as_deref()),
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
					error!("{}: {platform}/{handle} failed, skipping: {e:#}", person.name);
					pb.suspend(|| println!("   {} {name} {platform}/{handle}: {e:#}", "✗".red()));
				}
			}
		}

		let Some(delta) = delta::Delta::new(&person, &fetched_sources, messages, activity) else {
			info!("{}: nothing new", person.name);
			pb.suspend(|| println!("   {} {name} unchanged", "·".dimmed()));
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

		updated += 1;
		entries += extraction.new_log_entries.len();
		info!("{}: +{} log entries over {} sources", person.name, extraction.new_log_entries.len(), fetched_sources.len());
		let added = if added.is_empty() { String::new() } else { format!(", +{}", added.join(" +")) };
		pb.suspend(|| {
			println!(
				"   {} {name} +{} log entries, {} sources{added}",
				"✓".green(),
				extraction.new_log_entries.len(),
				fetched_sources.len()
			)
		});
		person.absorb(extraction.summary, extraction.new_log_entries, fetched_sources, handles);
		person.write(dir)?;
		// after the write, so a crash re-fetches rather than losing the messages
		for (platform, cursor) in cursors {
			db.set_rolodex_checkpoint(&person.name, &platform, &cursor).await?;
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
