#![doc = include_str!("README.md")]
mod delta;
mod person;
mod sources;

use std::{
	collections::BTreeMap,
	io::Write as _,
	path::{Path, PathBuf},
	process::{Command, Stdio},
};

use clap::{Args, Subcommand};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use futures::future::{Either, select};
use grammers_client::Client;
use person::Person;
use social_networks_utils::{
	db::Database,
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
};
use tracing::{error, info};

use crate::config::AppConfig;

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

async fn pull(config: AppConfig, dir: &Path, pattern: Option<&str>) -> Result<()> {
	let people: Vec<Person> = person::load_dir(dir)?.into_values().filter(|p| pattern.is_none_or(|pattern| p.matches(pattern))).collect();
	if people.is_empty() {
		bail!("no people in {} matching {}", dir.display(), pattern.unwrap_or("anything"));
	}
	let db = Database::try_new().await?;

	if !people.iter().any(|p| p.handles.contains_key("telegram")) {
		return pull_all(&config, &db, dir, people, None).await;
	}

	let TelegramConnection { client, mut runner, .. } = telegram_utils::connect(ConnectionConfig {
		username: &config.telegram.username,
		phone: &config.telegram.phone,
		api_id: config.telegram.api_id,
		api_hash: &config.telegram.api_hash,
		session_suffix: "_rolodex",
		seed_from: Some("_dm"),
	})
	.await?;
	match select(std::pin::pin!(pull_all(&config, &db, dir, people, Some(&client))), runner.as_mut()).await {
		Either::Left((result, _)) => result,
		Either::Right(((), _)) => bail!("MTProto runner exited during pull"),
	}
}

async fn pull_all(config: &AppConfig, db: &Database, dir: &Path, people: Vec<Person>, telegram: Option<&Client>) -> Result<()> {
	let discord = sources::Discord::new(config.dms.discord.user_token.clone(), config.dms.discord.my_username.clone());
	let github = sources::Github::default();

	for mut person in people {
		let mut fetched_sources = BTreeMap::new();
		let mut handles = BTreeMap::new();
		let mut messages = Vec::new();
		let mut activity = Vec::new();
		let mut cursors: Vec<(String, String)> = Vec::new();

		for (platform, handle) in &person.handles {
			let cursor = db.rolodex_checkpoint(&person.name, platform).await?;
			let result = match platform.as_str() {
				"discord" => discord.fetch(handle, cursor.as_deref()).await,
				"telegram" => sources::telegram(telegram.expect("a telegram client is connected iff somebody has a telegram handle"), handle, cursor.as_deref()).await,
				"github" => github.fetch(handle, cursor.as_deref()).await,
				// the remaining connected-account handles (youtube, battlenet, …) carry no fetch path
				_ => continue,
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
				Err(e) => error!("{}: {platform}/{handle} failed, skipping: {e:#}", person.name),
			}
		}

		let Some(delta) = delta::Delta::new(&person, &fetched_sources, messages, activity) else {
			info!("{}: nothing new", person.name);
			continue;
		};
		let extraction = delta::extract(&delta).await.wrap_err_with(|| format!("extraction for {}", person.name))?;

		println!("{}: +{} log entries", person.name, extraction.new_log_entries.len());
		person.absorb(extraction.summary, extraction.new_log_entries, fetched_sources, handles);
		person.write(dir)?;
		// after the write, so a crash re-fetches rather than losing the messages
		for (platform, cursor) in cursors {
			db.set_rolodex_checkpoint(&person.name, &platform, &cursor).await?;
		}
	}
	Ok(())
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
