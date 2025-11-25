mod config;
mod discord;
mod email;
mod telegram;
mod telegram_notifier;
mod twitter;
mod twitter_schedule;
mod twitter_user;
mod utils;
mod youtube;

use clap::{Args, Parser, Subcommand};
use config::AppConfig;
use v_utils::utils::exit_on_error;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
struct Cli {
	#[command(subcommand)]
	command: Commands,
	#[arg(long)]
	config: Option<v_utils::io::ExpandedPath>,
}

#[derive(Subcommand)]
enum Commands {
	/// Discord operations
	Discord(discord::DiscordArgs),
	/// Email operations
	Email(email::EmailArgs),
	/// Telegram operations
	Telegram(telegram::TelegramArgs),
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
	color_eyre::install().unwrap();
	let cli = Cli::parse();

	let config = exit_on_error(AppConfig::read(cli.config));
	let success = match cli.command {
		Commands::Discord(args) => discord::main(config, args),
		Commands::Email(args) => email::main(config, args),
		Commands::Telegram(args) => telegram::main(config, args),
		Commands::Twitter(args) => twitter::main(config, args),
		Commands::TwitterSchedule(args) => twitter_schedule::main(config, args),
		Commands::Youtube(args) => youtube::main(config, args),
	};
	exit_on_error(success);
}
