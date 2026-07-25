//! Confirms `telegram.owner_chat_id` is right — the single most likely misconfiguration.
//! `TELEGRAM_BOT_KEY=... TELEGRAM_OWNER_CHAT_ID=... cargo r -p social_networks_adapters --example notify_owner`
use social_networks_adapters::{telegram_dms::TelegramConfig, telegram_notifier::TelegramNotifier};

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	tracing_subscriber::fmt::init();
	let config = TelegramConfig {
		bot_token: std::env::var("TELEGRAM_BOT_KEY")?,
		owner_chat_id: std::env::var("TELEGRAM_OWNER_CHAT_ID")?.parse()?,
		..Default::default()
	};
	TelegramNotifier::new(config).report_recoverable("notify_owner_example", "test report").await;
	Ok(())
}
