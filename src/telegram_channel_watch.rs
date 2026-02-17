use std::{panic::AssertUnwindSafe, time::Duration};

use clap::Args;
use color_eyre::eyre::{Result, bail};
use futures::{
	FutureExt,
	future::{Either, select},
};
use grammers_client::{Client, Update};
use grammers_session::defs::PeerRef;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::{
	config::{AppConfig, TelegramDestination},
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
};

pub fn main(config: AppConfig, _args: TelegramArgs) -> Result<()> {
	println!("Starting Telegram Channel Watch...");
	v_utils::clientside!(Some("telegram_channel_watch"));

	// Increase stack size to handle deeply nested Telegram TL types
	// Default tokio stack is 2MB, increase to 8MB to prevent stack overflow on complex updates
	let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().thread_stack_size(8 * 1024 * 1024).build()?;
	runtime.block_on(async {
		let mut attempt = 0u32;
		//LOOP: daemon - runs until process termination
		loop {
			// Wrap in catch_unwind to recover from stack overflows and other panics
			let result = AssertUnwindSafe(run_telegram_monitor(&config)).catch_unwind().await;

			match result {
				Ok(Ok(())) => {
					// Clean exit (shouldn't happen in normal operation)
					attempt = 0;
				}
				Ok(Err(e)) => {
					let delay = reconnect_delay(attempt);
					error!("Telegram monitor error: {e}\nReconnecting in {:.1}s...", delay.as_secs_f64());
					tokio::time::sleep(delay).await;
					attempt += 1;
				}
				Err(panic_info) => {
					let panic_msg = if let Some(s) = panic_info.downcast_ref::<&str>() {
						s.to_string()
					} else if let Some(s) = panic_info.downcast_ref::<String>() {
						s.clone()
					} else {
						"unknown panic".to_string()
					};
					let delay = reconnect_delay(attempt);
					error!("Telegram monitor PANIC: {panic_msg}\nRestarting in {:.1}s...", delay.as_secs_f64());
					tokio::time::sleep(delay).await;
					attempt += 1;
				}
			}
		}
	})
}
#[derive(Args)]
pub struct TelegramArgs {}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatusDrop {
	status: String,
}

fn reconnect_delay(attempt: u32) -> Duration {
	let delay_secs = std::f64::consts::E.powi(attempt as i32).min(600.0); // cap at 10 min
	Duration::from_secs_f64(delay_secs)
}

async fn run_telegram_monitor(config: &AppConfig) -> Result<()> {
	// Load status drop
	let status_file = v_utils::xdg_state_file!("telegram_status.json");
	let status_drop: StatusDrop = if status_file.exists() {
		let content = std::fs::read_to_string(&status_file)?;
		let status: StatusDrop = serde_json::from_str(&content)?;
		info!("Loaded status from file: {}", status.status);
		status
	} else {
		info!("No status file found, using empty status");
		StatusDrop::default()
	};

	// Connect using shared utilities
	let TelegramConnection { client, mut updates, mut runner } = telegram_utils::connect(ConnectionConfig {
		username: &config.telegram.username,
		phone: &config.telegram.phone,
		api_id: config.telegram.api_id,
		api_hash: &config.telegram.api_hash,
		session_suffix: "",
	})
	.await?;

	println!("Telegram started");
	info!("--Telegram-- connected and authorized");

	// Resolve channel peer IDs
	info!("Resolving {} poll channels", config.telegram.poll_channels.len());
	let mut poll_peer_ids = Vec::new();
	for channel in &config.telegram.poll_channels {
		match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
			Some(peer) => {
				poll_peer_ids.push(peer.id());
				info!("Resolved poll channel: {channel} -> {}", peer.id().bot_api_dialog_id());
			}
			None => {
				error!("Could not resolve poll channel: {channel}");
			}
		}
	}

	info!("Resolving {} info channels", config.telegram.info_channels.len());
	let mut info_peer_ids = Vec::new();
	for channel in &config.telegram.info_channels {
		match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
			Some(peer) => {
				info_peer_ids.push(peer.id());
				info!("Resolved info channel: {channel} -> {}", peer.id().bot_api_dialog_id());
			}
			None => {
				error!("Could not resolve info channel: {channel}");
			}
		}
	}

	// Resolve output channel - extract username from TelegramDestination
	let output_username = match &config.telegram.channel_output {
		TelegramDestination::Channel(tg::TopLevelId::AtName(name)) | TelegramDestination::Group(tg::TopLevelId::AtName(name)) => name.trim_start_matches('@'),
		_ => {
			bail!("channel_output must be a username for grammers client forwarding");
		}
	};

	info!("Resolving output channel: {output_username}");
	let watch_chat = match client.resolve_username(output_username).await? {
		Some(peer) => {
			info!("Output channel resolved: {}", peer.id().bot_api_dialog_id());
			PeerRef::from(peer)
		}
		None => {
			error!("Could not resolve output channel: {output_username}");
			bail!("Could not resolve output channel: {output_username}");
		}
	};

	eprintln!("Listening for channel messages...");
	info!("Starting main event loop");

	// Main event loop
	let mut message_counter = 0u64;
	let mut last_status_update = Timestamp::default();

	//LOOP: terminates on error/bail, causing reconnect in outer daemon loop
	loop {
		// Check stack usage and bail if critical
		if telegram_utils::should_reconnect_for_stack() {
			bail!("Stack usage critical, forcing reconnect");
		}

		telegram_utils::log_stack("telegram_channel_watch loop start");

		enum Event {
			Update(Box<Result<Update, grammers_client::InvocationError>>),
			RunnerExited,
		}

		let event = {
			let update_fut = std::pin::pin!(updates.next());
			let runner_fut = runner.as_mut();

			match select(update_fut, runner_fut).await {
				Either::Left((result, _)) => Event::Update(Box::new(result)),
				Either::Right(((), _)) => Event::RunnerExited,
			}
		};

		telegram_utils::log_stack("telegram_channel_watch after select");

		match event {
			Event::RunnerExited => {
				bail!("MTProto runner exited unexpectedly");
			}
			Event::Update(result) => match *result {
				Err(e) => {
					error!("Error getting next update: {e}");
					continue;
				}
				Ok(update) => {
					message_counter += 1;

					match update {
						Update::NewMessage(message) if !message.outgoing() => {
							let peer = match message.peer() {
								Ok(p) => p,
								Err(e) => {
									error!("Skipping message with unresolved peer: {e:?}");
									continue;
								}
							};
							let peer_id = peer.id();

							// Check if it's from a monitored channel
							if poll_peer_ids.contains(&peer_id) {
								if let Err(e) = handle_poll_message(&client, &message, watch_chat).await {
									error!("Error handling poll message: {e}");
								}
							} else if info_peer_ids.contains(&peer_id)
								&& let Err(e) = handle_info_message(&client, &message, watch_chat).await
							{
								error!("Error handling info message: {e}");
							}
						}
						_ => {}
					}

					// Status update every 4 minutes
					let now = Timestamp::now();
					if now.duration_since(last_status_update) > SignedDuration::from_secs(4 * 60) {
						if !status_drop.status.is_empty() {
							if let Err(e) = update_profile(&client, &status_drop.status).await {
								error!("Error updating profile: {e}");
							} else {
								debug!("Profile status updated; message counter: {message_counter}");
							}
						}
						last_status_update = now;
					}
				}
			},
		}
	}
}

async fn handle_poll_message(client: &Client, message: &grammers_client::types::Message, watch_chat: PeerRef) -> Result<()> {
	// Check if message contains a poll or media
	if message.media().is_some() {
		// Forward poll messages to watch channel
		let source = match message.peer() {
			Ok(p) => p,
			Err(e) => {
				error!("Cannot forward poll message - unresolved peer: {e:?}");
				return Ok(());
			}
		};
		client.forward_messages(watch_chat, &[message.id()], PeerRef::from(source.clone())).await?;
		info!("Forwarded poll/media message from {}", source.name().unwrap_or("unknown"));
	}
	Ok(())
}

async fn handle_info_message(client: &Client, message: &grammers_client::types::Message, watch_chat: PeerRef) -> Result<()> {
	let key_words = [
		"самые торгуемые акции",
		"отслеживание настроений",
		"гугл тренд",
		"google trends",
		"поисковых запросов",
		"популярные запросы",
		"популярных запросов",
		"кредитное плечо",
		"закредитованность",
		"количество уникальных слов",
		"открытому интересу",
		"открытый интерес",
	];

	let text = message.text();
	let text_lower = text.to_lowercase();
	if key_words.iter().any(|word| text_lower.contains(word)) {
		// Forward message to watch channel
		let source = match message.peer() {
			Ok(p) => p,
			Err(e) => {
				error!("Cannot forward info message - unresolved peer: {e:?}");
				return Ok(());
			}
		};
		client.forward_messages(watch_chat, &[message.id()], PeerRef::from(source.clone())).await?;
		info!("Forwarded info message from {}", source.name().unwrap_or("unknown"));
	}
	Ok(())
}

async fn update_profile(client: &Client, status: &str) -> Result<()> {
	use grammers_tl_types::functions;

	client
		.invoke(&functions::account::UpdateProfile {
			first_name: None,
			last_name: None,
			about: Some(status.to_string()),
		})
		.await?;

	Ok(())
}
