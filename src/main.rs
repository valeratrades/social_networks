mod config;
mod db;
mod dms;
mod email;
mod telegram_channel_watch;
mod telegram_notifier;
mod twitter;
mod twitter_schedule;
mod twitter_user;
mod utils;
mod youtube;

use clap::{Args, Parser, Subcommand};
use config::{AppConfig, LiveSettings, SettingsFlags};
use v_utils::utils::exit_on_error;

#[derive(Parser)]
#[command(author, version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("GIT_HASH"), ")"), about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
	#[command(flatten)]
	settings: SettingsFlags,
}

#[derive(Subcommand)]
enum Commands {
	/// DM monitoring (ping, monitored users) for Discord and Telegram simultaneously
	Dms(dms::DmsArgs),
	/// Email operations
	Email(email::EmailArgs),
	/// Run database migrations
	MigrateDb,
	/// Telegram channel watching (poll/info forwarding)
	TelegramChannelWatch(telegram_channel_watch::TelegramArgs),
	/// Twitter operations
	Twitter(twitter::TwitterArgs),
	/// Twitter scheduled posting
	TwitterSchedule(twitter_schedule::TwitterScheduleArgs),
	/// YouTube operations
	Youtube(youtube::YoutubeArgs),
}

#[derive(Args)]
struct NoArgs {}

fn main() {
	let cli = Cli::parse();

	let settings = exit_on_error(LiveSettings::new(cli.settings, std::time::Duration::from_secs(60)));
	let config: AppConfig = exit_on_error(settings.config());

	let success = match cli.command {
		Commands::Dms(args) => dms::main(config, args),
		Commands::Email(args) => email::main(config, args),
		Commands::MigrateDb => {
			let db = db::Database::new(&config.clickhouse);
			let runtime = tokio::runtime::Runtime::new().unwrap();
			runtime.block_on(async { db.migrate().await })
		}
		Commands::TelegramChannelWatch(args) => telegram_channel_watch::main(config, args),
		Commands::Twitter(args) => twitter::main(config, args),
		Commands::TwitterSchedule(args) => twitter_schedule::main(config, args),
		Commands::Youtube(args) => youtube::main(config, args),
	};
	exit_on_error(success);
}
