use std::convert::Infallible;

use clap::Args;
use color_eyre::eyre::Result;
use futures::future::{Either, select};
use grammers_client::{Client, update::Update};
use grammers_session::types::PeerRef;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

use crate::{
	client::{AdapterError, Client as AdapterClient},
	telegram_dms::{TelegramConfig, TelegramDestination, classify_invocation_auth, classify_telegram_auth_error},
};

const SURFACE: &str = "telegram_channel_watch";
#[derive(Args)]
pub struct TelegramArgs {}

pub struct TelegramChannelWatch {
	telegram_config: TelegramConfig,
}

impl TelegramChannelWatch {
	pub fn new(telegram_config: TelegramConfig) -> Self {
		Self { telegram_config }
	}
}

impl AdapterClient for TelegramChannelWatch {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		println!("Starting Telegram Channel Watch...");
		let mut attempt: u32 = 0;
		loop {
			match run_telegram_monitor(&self.telegram_config).await {
				Err(ChannelWatchError::Auth(detail)) => return Err(AdapterError::Auth { surface: SURFACE, detail }),
				Err(ChannelWatchError::Recoverable(e)) => {
					let delay = reconnect_delay(attempt);
					error!("Telegram monitor error: {e:#}\nReconnecting in {:.1}s...", delay.as_secs_f64());
					time::sleep(delay).await;
					attempt = attempt.saturating_add(1);
				}
			}
		}
	}
}

enum ChannelWatchError {
	Auth(String),
	Recoverable(color_eyre::eyre::Report),
}

impl<E: Into<color_eyre::eyre::Report>> From<E> for ChannelWatchError {
	fn from(e: E) -> Self {
		ChannelWatchError::Recoverable(e.into())
	}
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatusDrop {
	status: String,
}

fn reconnect_delay(attempt: u32) -> Duration {
	let delay_secs = std::f64::consts::E.powi(attempt as i32).min(600.0);
	Duration::from_secs_f64(delay_secs)
}

async fn run_telegram_monitor(telegram_config: &TelegramConfig) -> Result<Infallible, ChannelWatchError> {
	let status_file = xdg::BaseDirectories::with_prefix("social_networks")
		.place_state_file("telegram_status.json")
		.map_err(color_eyre::eyre::Report::from)?;
	let status_drop: StatusDrop = if status_file.exists() {
		let content = std::fs::read_to_string(&status_file).map_err(color_eyre::eyre::Report::from)?;
		let status: StatusDrop = serde_json::from_str(&content).map_err(color_eyre::eyre::Report::from)?;
		info!("Loaded status from file: {}", status.status);
		status
	} else {
		info!("No status file found, using empty status");
		StatusDrop::default()
	};

	let TelegramConnection { client, mut updates, mut runner } = telegram_utils::connect(ConnectionConfig {
		username: &telegram_config.username,
		phone: &telegram_config.phone,
		api_id: telegram_config.api_id,
		api_hash: &telegram_config.api_hash,
		session_suffix: "",
	})
	.await
	.map_err(|e| {
		if let Some(detail) = classify_telegram_auth_error(&e) {
			ChannelWatchError::Auth(detail)
		} else {
			ChannelWatchError::Recoverable(e)
		}
	})?;

	println!("Telegram started");
	info!("--Telegram-- connected and authorized");

	info!("Resolving {} poll channels", telegram_config.poll_channels.len());
	let mut poll_peer_ids = Vec::new();
	for channel in &telegram_config.poll_channels {
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

	info!("Resolving {} info channels", telegram_config.info_channels.len());
	let mut info_peer_ids = Vec::new();
	for channel in &telegram_config.info_channels {
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

	let output_username = match &telegram_config.channel_output {
		TelegramDestination::Channel(tg::TopLevelId::AtName(name)) | TelegramDestination::Group(tg::TopLevelId::AtName(name)) => name.trim_start_matches('@'),
		_ => {
			return Err(ChannelWatchError::Recoverable(color_eyre::eyre::eyre!(
				"channel_output must be a username for grammers client forwarding"
			)));
		}
	};

	info!("Resolving output channel: {output_username}");
	let watch_chat = match client.resolve_username(output_username).await? {
		Some(peer) => {
			info!("Output channel resolved: {}", peer.id().bot_api_dialog_id());
			match peer.to_ref().await {
				Some(r) => r,
				None =>
					return Err(ChannelWatchError::Recoverable(color_eyre::eyre::eyre!(
						"Output channel peer has no access hash: {output_username}"
					))),
			}
		}
		None => {
			error!("Could not resolve output channel: {output_username}");
			return Err(ChannelWatchError::Recoverable(color_eyre::eyre::eyre!("Could not resolve output channel: {output_username}")));
		}
	};

	eprintln!("Listening for channel messages...");
	info!("Starting main event loop");

	let mut message_counter = 0u64;
	let mut last_status_update = Timestamp::default();

	loop {
		if telegram_utils::should_reconnect_for_stack() {
			return Err(ChannelWatchError::Recoverable(color_eyre::eyre::eyre!("Stack usage critical, forcing reconnect")));
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
				return Err(ChannelWatchError::Recoverable(color_eyre::eyre::eyre!("MTProto runner exited unexpectedly")));
			}
			Event::Update(result) => match *result {
				Err(e) => {
					let s = format!("{e:#}");
					if classify_invocation_auth(&s) {
						return Err(ChannelWatchError::Auth(s));
					}
					error!("Error getting next update: {s}");
					continue;
				}
				Ok(update) => {
					message_counter += 1;

					match update {
						Update::NewMessage(message) if !message.outgoing() => {
							let peer = match message.peer() {
								Some(p) => p,
								None => {
									error!("Skipping message with unresolved peer");
									continue;
								}
							};
							let peer_id = peer.id();

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

async fn handle_poll_message(client: &Client, message: &grammers_client::update::Message, watch_chat: PeerRef) -> Result<()> {
	if message.media().is_some() {
		let source_ref = match message.peer_ref().await {
			Some(r) => r,
			None => {
				error!("Cannot forward poll message - unresolved peer");
				return Ok(());
			}
		};
		let source_name = message.peer().and_then(|p| p.name()).unwrap_or("unknown").to_string();
		client.forward_messages(watch_chat, &[message.id()], source_ref).await?;
		info!("Forwarded poll/media message from {source_name}");
	}
	Ok(())
}

async fn handle_info_message(client: &Client, message: &grammers_client::update::Message, watch_chat: PeerRef) -> Result<()> {
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
		let source_ref = match message.peer_ref().await {
			Some(r) => r,
			None => {
				error!("Cannot forward info message - unresolved peer");
				return Ok(());
			}
		};
		let source_name = message.peer().and_then(|p| p.name()).unwrap_or("unknown").to_string();
		client.forward_messages(watch_chat, &[message.id()], source_ref).await?;
		info!("Forwarded info message from {source_name}");
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
