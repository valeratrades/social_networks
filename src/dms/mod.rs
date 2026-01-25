pub fn main(config: AppConfig, _args: DmsArgs) -> Result<()> {
	v_utils::clientside!("dms");

	println!("DMs: Starting Discord and Telegram monitors...");

	// Increase stack size to handle deeply nested Telegram TL types
	// Default tokio stack is 2MB, increase to 8MB to prevent stack overflow on complex updates
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(8 * 1024 * 1024).build()?;
	runtime.block_on(run(config))
}
#[derive(Args)]
pub struct DmsArgs {}
mod discord;
mod telegram;

use std::{panic::AssertUnwindSafe, pin::Pin, time::Duration};

use clap::Args;
use color_eyre::eyre::Result;
use futures::FutureExt;
use futures_util::{StreamExt, stream::FuturesUnordered};

use crate::config::AppConfig;

fn reconnect_delay(attempt: u32) -> Duration {
	let delay_secs = std::f64::consts::E.powi(attempt as i32).min(600.0); // cap at 10 min
	Duration::from_secs_f64(delay_secs)
}

enum MonitorInstance {
	Discord(Box<discord::DiscordMonitor>, u32), // includes retry attempt count
	Telegram(Box<telegram::TelegramMonitor>, u32),
}

async fn run(config: AppConfig) -> Result<()> {
	let discord_monitor = discord::DiscordMonitor::new(config.clone());
	let telegram_monitor = telegram::TelegramMonitor::new(config);

	type BoxFut = Pin<Box<dyn std::future::Future<Output = (MonitorInstance, std::result::Result<Result<()>, Box<dyn std::any::Any + Send>>)>>>;
	let mut futures: FuturesUnordered<BoxFut> = FuturesUnordered::new();

	futures.push(Box::pin(async move {
		let mut m = discord_monitor;
		let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
		(MonitorInstance::Discord(Box::new(m), 0), result)
	}));

	futures.push(Box::pin(async move {
		let mut m = telegram_monitor;
		let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
		(MonitorInstance::Telegram(Box::new(m), 0), result)
	}));

	while let Some((instance, result)) = futures.next().await {
		match (instance, result) {
			// Discord success
			(MonitorInstance::Discord(mut m, _), Ok(Ok(()))) => {
				futures.push(Box::pin(async move {
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Discord(m, 0), result)
				}));
			}
			// Discord error
			(MonitorInstance::Discord(mut m, attempt), Ok(Err(e))) => {
				let delay = reconnect_delay(attempt);
				tracing::error!("Discord monitor error: {e}, retrying in {:.1}s", delay.as_secs_f64());
				futures.push(Box::pin(async move {
					tokio::time::sleep(delay).await;
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Discord(m, attempt + 1), result)
				}));
			}
			// Discord panic
			(MonitorInstance::Discord(mut m, attempt), Err(panic_info)) => {
				let panic_msg = extract_panic_msg(&panic_info);
				let delay = reconnect_delay(attempt);
				tracing::error!("Discord monitor PANIC: {panic_msg}, restarting in {:.1}s", delay.as_secs_f64());
				futures.push(Box::pin(async move {
					tokio::time::sleep(delay).await;
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Discord(m, attempt + 1), result)
				}));
			}
			// Telegram success
			(MonitorInstance::Telegram(mut m, _), Ok(Ok(()))) => {
				futures.push(Box::pin(async move {
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Telegram(m, 0), result)
				}));
			}
			// Telegram error
			(MonitorInstance::Telegram(mut m, attempt), Ok(Err(e))) => {
				let delay = reconnect_delay(attempt);
				tracing::error!("Telegram monitor error: {e}, retrying in {:.1}s", delay.as_secs_f64());
				futures.push(Box::pin(async move {
					tokio::time::sleep(delay).await;
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Telegram(m, attempt + 1), result)
				}));
			}
			// Telegram panic
			(MonitorInstance::Telegram(mut m, attempt), Err(panic_info)) => {
				let panic_msg = extract_panic_msg(&panic_info);
				let delay = reconnect_delay(attempt);
				tracing::error!("Telegram monitor PANIC: {panic_msg}, restarting in {:.1}s", delay.as_secs_f64());
				futures.push(Box::pin(async move {
					tokio::time::sleep(delay).await;
					let result = AssertUnwindSafe(m.collect()).catch_unwind().await;
					(MonitorInstance::Telegram(m, attempt + 1), result)
				}));
			}
		}
	}

	Ok(())
}

fn extract_panic_msg(panic_info: &Box<dyn std::any::Any + Send>) -> String {
	if let Some(s) = panic_info.downcast_ref::<&str>() {
		s.to_string()
	} else if let Some(s) = panic_info.downcast_ref::<String>() {
		s.clone()
	} else {
		"unknown panic".to_string()
	}
}
