//! Confirms the owner's `@username` resolves to a DM the bot can actually deliver.
//! `TELEGRAM_BOT_KEY=... cargo r -p social_networks_adapters --example notify_owner @valeratrades`
use social_networks_adapters::{telegram_dms::TelegramConfig, telegram_notifier::TelegramNotifier};

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt::init();
	let config = TelegramConfig {
		bot_token: std::env::var("TELEGRAM_BOT_KEY")?,
		username: std::env::args().nth(1).ok_or_else(|| color_eyre::eyre::eyre!("pass the owner's @username"))?,
		..Default::default()
	};
	TelegramNotifier::new(config).report_recoverable("notify_owner_example", "test report").await;
	Ok(())
}
