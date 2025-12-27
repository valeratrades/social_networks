mod discord;
mod telegram;

use std::pin::Pin;

use clap::Args;
use color_eyre::eyre::Result;
use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::config::AppConfig;

#[derive(Args)]
pub struct DmsArgs {}

pub fn main(config: AppConfig, _args: DmsArgs) -> Result<()> {
	v_utils::clientside!("dms");

	println!("DMs: Starting Discord and Telegram monitors...");

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(run(config))
}

enum MonitorInstance {
	Discord(discord::DiscordMonitor),
	Telegram(telegram::TelegramMonitor),
}

async fn run(config: AppConfig) -> Result<()> {
	let discord_monitor = discord::DiscordMonitor::new(config.clone());
	let telegram_monitor = telegram::TelegramMonitor::new(config);

	type BoxFut = Pin<Box<dyn std::future::Future<Output = (MonitorInstance, Result<()>)>>>;
	let mut futures: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	futures.push(Box::pin(async move {
		let mut m = discord_monitor;
		let result = m.collect().await;
		(MonitorInstance::Discord(m), result)
	}));

	futures.push(Box::pin(async move {
		let mut m = telegram_monitor;
		let result = m.collect().await;
		(MonitorInstance::Telegram(m), result)
	}));

	while let Some((instance, result)) = futures.next().await {
		if let Err(e) = result {
			tracing::error!("Monitor error: {e}");
		}

		match instance {
			MonitorInstance::Discord(m) => {
				futures.push(Box::pin(async move {
					let mut m = m;
					let result = m.collect().await;
					(MonitorInstance::Discord(m), result)
				}));
			}
			MonitorInstance::Telegram(m) => {
				futures.push(Box::pin(async move {
					let mut m = m;
					let result = m.collect().await;
					(MonitorInstance::Telegram(m), result)
				}));
			}
		}
	}

	Ok(())
}
