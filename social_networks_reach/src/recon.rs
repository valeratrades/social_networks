//! The venue axis, hand-run.
//!
//! Never invoked by a daemon: every call here spends somebody's rate limit and account standing, and
//! the way to keep that bounded is to keep a human in front of it. That is also why it is a binary of
//! this crate rather than a subcommand of `social_networks` — the app never wires it, so it cannot
//! expose it.
//!
//! ```text
//! recon venues  <platform>                            what this session can see
//! recon members <platform>:<slug>                   → members.json
//! recon posts   <platform>:<slug> --since <tf>      → <year>.md
//! recon roster  <platform>:<slug> [--where …]         read the roster back
//! ```

use std::path::Path;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr, eyre};
use colored::Colorize as _;
use jiff::{SignedDuration, Timestamp};
use social_networks_adapters::{
	github::Github,
	reach::{Venue, VenueRef, VenueSource, Window},
	skool::{Skool, SkoolCredentials},
	telegram_dms::{self, TelegramConfig},
};
use social_networks_reach::{
	RolodexConfig,
	venue::{self, Store},
	with_telegram,
};
use v_utils::{
	Timeframe,
	macros::{MyConfigPrimitives, Settings},
};

/// The sections of `~/.config/social_networks` this axis reads. A partial view of the same file the
/// daemon uses: `recon` has no state of its own to configure.
#[derive(Clone, Debug, Default, MyConfigPrimitives, Settings)]
#[settings(config_name = "social_networks")]
pub struct ReconConfig {
	#[settings(skip)]
	#[serde(default)]
	pub telegram: TelegramConfig,
	#[settings(skip)]
	#[serde(default)]
	pub skool: Option<SkoolCredentials>,
	#[settings(skip)]
	#[serde(default)]
	pub rolodex: Option<RolodexConfig>,
}

#[derive(Parser)]
#[command(author, version, about = "read a venue's roster and transcript", long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Command,
	#[command(flatten)]
	settings: SettingsFlags,
}

#[derive(Subcommand)]
enum Command {
	/// List the venues this session can reach on a platform
	Venues { platform: VenueSource },
	/// Write `<platform>:<slug>`'s roster to `members.json`
	Members {
		#[arg(value_parser = venue_ref)]
		at: VenueRef,
	},
	/// Append `<platform>:<slug>`'s posts to its year files
	Posts {
		#[arg(value_parser = venue_ref)]
		at: VenueRef,
		/// How far back to read. Without it, from the last checkpoint.
		#[arg(long)]
		since: Option<Timeframe>,
	},
	/// Read a written roster back, optionally filtered
	Roster {
		#[arg(value_parser = venue_ref)]
		at: VenueRef,
		/// A SQL `WHERE` clause over the roster table, or a path to a file holding one
		#[arg(long = "where")]
		predicate: Option<String>,
		#[arg(long)]
		json: bool,
	},
}
impl Command {
	fn platform(&self) -> VenueSource {
		match self {
			Self::Venues { platform } => *platform,
			Self::Members { at } | Self::Posts { at, .. } | Self::Roster { at, .. } => at.platform,
		}
	}
}

fn main() -> Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt().with_env_filter(std::env::var("RUST_LOG").unwrap_or_else(|_| "warn".to_string())).init();
	let cli = Cli::parse();
	let config = ReconConfig::try_build(cli.settings).map_err(|e| eyre!("{e}"))?;
	let dir = config
		.rolodex
		.as_ref()
		.ok_or_else(|| eyre!("no `[rolodex]` section in the config — a venue transcript lives under its `path`"))?
		.path
		.clone();

	// telegram TL types are deep enough to need the same 8 MiB the daemon provisions
	tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.thread_stack_size(8 * 1024 * 1024)
		.build()
		.expect("failed to build tokio runtime")
		.block_on(run(&config, &dir, cli.command))
}

/// One match over [`VenueSource`], so a platform that gains a venue stops the build here rather than
/// falling through to an arm that reads nothing.
async fn run(config: &ReconConfig, dir: &Path, command: Command) -> Result<()> {
	match command.platform() {
		VenueSource::Skool => {
			let creds = config
				.skool
				.as_ref()
				.ok_or_else(|| eyre!("a skool group is only readable by a member of it, so this needs a `[skool]` section in the config"))?;
			act(&mut Skool::try_new(Some(creds.clone()))?, dir, command).await
		}
		VenueSource::Github => act(&mut Github::default(), dir, command).await,
		VenueSource::Telegram => with_telegram(&config.telegram, async |client| act(&mut telegram_dms::Reach { client: &client }, dir, command).await).await,
	}
}

async fn act<V: Venue>(client: &mut V, dir: &Path, command: Command) -> Result<()> {
	match command {
		Command::Venues { platform } => {
			let venues = client.venues().await?;
			if venues.is_empty() {
				println!("   {} nothing this session can enumerate on {}", "·".dimmed(), platform.as_ref());
			}
			for at in venues {
				println!("{at}\t{}", at.display);
			}
		}
		Command::Members { at } => {
			let members = client.members(&at).await?;
			let store = Store::open(dir, &at)?;
			store.put_roster(&members)?;
			println!("   {} {} members → {}", "✓".green(), members.len(), store.dir().join("members.json").display());
		}
		Command::Posts { at, since } => {
			let mut store = Store::open(dir, &at)?;
			let window = match since {
				Some(tf) => Window::since(Timestamp::now() - SignedDuration::try_from(tf.duration()).wrap_err("a --since is milliseconds")?),
				None => Window::above(store.cursor().map(str::to_string)),
			};
			let page = client.posts(&at, window, &store.dir().join("assets")).await?;
			let landed = store.record(page)?;
			println!("   {} +{landed} items → {}", "✓".green(), store.dir().display());
		}
		Command::Roster { at, predicate, json } => {
			let store = Store::open(dir, &at)?;
			let members = store.roster()?;
			let chosen = match predicate {
				None => members,
				Some(predicate) => venue::select(&members, &store.lines(None)?, &clause(&predicate)?).await?,
			};
			match json {
				true => println!("{}", serde_json::to_string_pretty(&chosen)?),
				false =>
					for member in &chosen {
						println!("{}\t{}\t{}", member.handle, member.display, member.joined.map(|t| t.to_string()).unwrap_or_default());
					},
			}
		}
	}
	Ok(())
}

fn venue_ref(s: &str) -> std::result::Result<VenueRef, String> {
	s.parse().map_err(|e| format!("{e:#}"))
}

/// Inline SQL, or a path to a file holding it — resolved by asking whether the argument names a file,
/// so anything worth an LSP can be written in one.
fn clause(predicate: &str) -> Result<String> {
	let path = Path::new(predicate);
	match path.is_file() {
		true => std::fs::read_to_string(path).wrap_err_with(|| format!("failed to read {}", path.display())),
		false => Ok(predicate.to_string()),
	}
}
