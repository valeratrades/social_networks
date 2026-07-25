//! Backfill the last hour of Discord DMs and print the resulting `DmEvent`s.
//! `DISCORD_AUTH=... DEFAULT_USERNAME=... cargo r -p social_networks_adapters --example discord_backfill`
use jiff::{SignedDuration, Timestamp};
use social_networks_adapters::{DiscordDms, discord::DiscordConfig, telegram_notifier::TelegramNotifier};
use v_utils::trades::{Timeframe, TimeframeDesignator};

#[tokio::main]
async fn main() -> color_eyre::eyre::Result<()> {
	color_eyre::install()?;
	let discord_config = DiscordConfig {
		user_token: std::env::var("DISCORD_AUTH")?,
		my_username: std::env::var("DEFAULT_USERNAME")?,
	};

	let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
	let notifier = TelegramNotifier::new(Default::default());
	let discord = DiscordDms::new(discord_config, tx, notifier, Timeframe::from_naive(1, TimeframeDesignator::Hours));
	discord.backfill(Timestamp::now() - SignedDuration::from_hours(1)).await?;
	drop(discord);

	while let Some(event) = rx.recv().await {
		println!("{event:?}");
	}
	Ok(())
}
