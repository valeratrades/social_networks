mod health;

use clap::{Parser, Subcommand};
use color_eyre::eyre::{ContextCompat, Result};
use social_networks_adapters::{
	AdapterError, Client, DiscordDms, EmailMonitor, TelegramChannelWatch, TelegramDms, TwitterMonitor, TwitterSchedule, YoutubeMonitor, alert, discord::DmsArgs, email::EmailArgs,
	telegram_channel_watch::TelegramArgs, twitter::TwitterArgs, twitter_schedule::TwitterScheduleArgs, youtube::YoutubeArgs,
};
use social_networks_utils::{
	config::{AppConfig, LiveSettings, SettingsFlags},
	db::Database,
};
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
	Dms(DmsArgs),
	/// Email operations
	Email(EmailArgs),
	/// Show health status of all services, config, and directories
	Health,
	/// Run database migrations
	MigrateDb,
	/// Telegram channel watching (poll/info forwarding)
	TelegramChannelWatch(TelegramArgs),
	/// Twitter operations
	Twitter(TwitterArgs),
	/// Twitter scheduled posting
	TwitterSchedule(TwitterScheduleArgs),
	/// YouTube operations
	Youtube(YoutubeArgs),
}

fn main() {
	let cli = Cli::parse();
	let settings = exit_on_error(LiveSettings::new(cli.settings, std::time::Duration::from_secs(60)));
	let config: AppConfig = exit_on_error(settings.config());

	let result: Result<()> = match cli.command {
		Commands::Health => health::main(config),
		Commands::MigrateDb => {
			let runtime = tokio::runtime::Runtime::new().unwrap();
			runtime.block_on(async { Database::try_new().await.map(|_| ()) })
		}
		Commands::Dms(_) => run_async(|| async {
			v_utils::clientside!(Some("dms"));
			let mut discord = DiscordDms::new(config.clone());
			let mut telegram = TelegramDms::new(config);
			let err = tokio::select! {
				e = discord.listen() => e.unwrap_err(),
				e = telegram.listen() => e.unwrap_err(),
			};
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
		Commands::Email(args) => run_async(|| async {
			v_utils::clientside!(Some("email"));
			let mut monitor = EmailMonitor::try_from_app_config(config)
				.await
				.map_err(adapter_from_eyre)?
				.context("Email config not found in config file")
				.map_err(adapter_from_eyre)?;
			if args.mark_all_read {
				return monitor.mark_all_as_read().await.map_err(adapter_from_eyre);
			}
			let err = monitor.listen().await.unwrap_err();
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
		Commands::TelegramChannelWatch(_) => run_async(|| async {
			v_utils::clientside!(Some("telegram_channel_watch"));
			let mut adapter = TelegramChannelWatch::new(config);
			let err = adapter.listen().await.unwrap_err();
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
		Commands::Twitter(_) => run_async(|| async {
			v_utils::clientside!(Some("twitter"));
			let mut adapter = TwitterMonitor::new(config);
			let err = adapter.listen().await.unwrap_err();
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
		Commands::TwitterSchedule(args) => run_async(|| async {
			v_utils::clientside!(Some("twitter_schedule"));
			let mut adapter = TwitterSchedule::new(config, args.skip_first);
			let err = adapter.listen().await.unwrap_err();
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
		Commands::Youtube(_) => run_async(|| async {
			v_utils::clientside!(Some("youtube"));
			let mut adapter = YoutubeMonitor::new(config);
			let err = adapter.listen().await.unwrap_err();
			alert(&err).await;
			Err::<(), AdapterError>(err)
		}),
	};

	exit_on_error(result);
}

/// Build a multi-thread runtime with a 8 MiB worker stack (telegram TL types are deep)
/// and run the given async block to completion.
fn run_async<F, Fut, T, E>(f: F) -> Result<T>
where
	F: FnOnce() -> Fut,
	Fut: std::future::Future<Output = Result<T, E>>,
	E: Into<color_eyre::eyre::Report>, {
	let runtime = tokio::runtime::Builder::new_multi_thread()
		.enable_all()
		.thread_stack_size(8 * 1024 * 1024)
		.build()
		.expect("failed to build tokio runtime");
	runtime.block_on(async { f().await.map_err(|e| e.into()) })
}

fn adapter_from_eyre(e: color_eyre::eyre::Report) -> AdapterError {
	AdapterError::Unhandled {
		surface: "email",
		detail: format!("{e:#}"),
	}
}
