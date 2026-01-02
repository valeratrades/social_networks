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

	// Increase stack size to handle deeply nested Telegram TL types
	// Default tokio stack is 2MB, increase to 8MB to prevent stack overflow on complex updates
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(8 * 1024 * 1024).build()?;
	runtime.block_on(run(config))
}

enum MonitorInstance {
	Discord(Box<discord::DiscordMonitor>),
	Telegram(Box<telegram::TelegramMonitor>),
}

async fn run(config: AppConfig) -> Result<()> {
	let discord_monitor = discord::DiscordMonitor::new(config.clone());
	let telegram_monitor = telegram::TelegramMonitor::new(config);

	type BoxFut = Pin<Box<dyn std::future::Future<Output = (MonitorInstance, Result<()>)>>>;
	let mut futures: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	futures.push(Box::pin(async move {
		let mut m = discord_monitor;
		let result = m.collect().await;
		(MonitorInstance::Discord(Box::new(m)), result)
	}));

	futures.push(Box::pin(async move {
		let mut m = telegram_monitor;
		let result = m.collect().await;
		(MonitorInstance::Telegram(Box::new(m)), result)
	}));

	while let Some((instance, result)) = futures.next().await {
		if let Err(e) = result {
			tracing::error!("Monitor error: {e}");
		}

		match instance {
			MonitorInstance::Discord(mut m) => {
				futures.push(Box::pin(async move {
					let result = m.collect().await;
					(MonitorInstance::Discord(m), result)
				}));
			}
			MonitorInstance::Telegram(mut m) => {
				futures.push(Box::pin(async move {
					let result = m.collect().await;
					(MonitorInstance::Telegram(m), result)
				}));
			}
		}
	}

	Ok(())
}
