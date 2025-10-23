use std::{collections::HashMap, sync::Arc};

use clap::Args;
use color_eyre::eyre::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use jiff::{Timestamp, fmt::strtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
	sync::Mutex,
	time::{self, Duration},
};
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

#[derive(Args)]
pub struct DiscordArgs {}

#[derive(Debug, Deserialize, Serialize)]
struct DiscordMessage {
	op: u8,
	d: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	s: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	t: Option<String>,
}

pub fn main(config: AppConfig, _args: DiscordArgs) -> Result<()> {
	// Set up tracing with file logging (truncate old logs)
	let log_file = v_utils::xdg_state_file!("discord.log");
	if log_file.exists() {
		std::fs::remove_file(&log_file)?;
	}
	let file_appender = tracing_appender::rolling::never(log_file.parent().unwrap(), log_file.file_name().unwrap());
	let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

	tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_max_level(tracing::Level::DEBUG).init();

	println!("Discord: Listening...");

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_discord_monitor(&config).await {
				error!("Discord monitor error: {e}");
				error!("Reconnecting in 5 minutes...");
				time::sleep(Duration::from_secs(5 * 60)).await;
			}
		}
	})
}

async fn run_discord_monitor(config: &AppConfig) -> Result<()> {
	let url = "wss://gateway.discord.gg/?v=6&encoding=json";
	let (ws_stream, _) = connect_async(url).await.context("Failed to connect to Discord WebSocket")?;

	let (write, read) = ws_stream.split();
	let write = Arc::new(Mutex::new(write));

	let mut read = read;

	// Receive the initial hello message
	let hello_msg = read.next().await.ok_or_else(|| color_eyre::eyre::eyre!("No hello message"))??;
	let hello: DiscordMessage = serde_json::from_str(&hello_msg.to_string())?;

	let heartbeat_interval = hello
		.d
		.as_ref()
		.and_then(|d| d.get("heartbeat_interval"))
		.and_then(|v| v.as_u64())
		.ok_or_else(|| color_eyre::eyre::eyre!("No heartbeat interval"))?;

	let heartbeat_interval_secs = heartbeat_interval / 1000;

	// Start heartbeat task
	let write_clone = Arc::clone(&write);
	let message_counter = Arc::new(Mutex::new(0u64));
	let message_counter_clone = Arc::clone(&message_counter);

	tokio::spawn(async move {
		let mut interval = time::interval(Duration::from_secs(heartbeat_interval_secs));
		loop {
			interval.tick().await;
			let heartbeat = DiscordMessage {
				op: 1,
				d: Some(json!(null)),
				s: None,
				t: None,
			};

			let msg = serde_json::to_string(&heartbeat).unwrap();
			let mut write = write_clone.lock().await;
			if write.send(Message::Text(msg.into())).await.is_err() {
				break;
			}
		}
	});

	// Send identify payload
	let identify = DiscordMessage {
		op: 2,
		d: Some(json!({
			"token": config.discord.user_token,
			"properties": {
				"$os": "linux",
				"$browser": "rust",
				"$device": "pc"
			}
		})),
		s: None,
		t: None,
	};

	let msg = serde_json::to_string(&identify)?;
	write.lock().await.send(Message::Text(msg.into())).await?;

	info!("--Discord-- connected to WebSocket");

	let telegram = TelegramNotifier::new(config.telegram.clone());
	// Track last message timestamp per channel for cooldown (channel_id -> timestamp)
	let last_message_times: Arc<Mutex<HashMap<String, Timestamp>>> = Arc::new(Mutex::new(HashMap::new()));

	// Main event loop
	while let Some(msg) = read.next().await {
		let msg = msg?;
		if let Message::Text(text) = msg
			&& let Ok(event) = serde_json::from_str::<DiscordMessage>(&text)
		{
			*message_counter_clone.lock().await += 1;

			match event.op {
				11 => {
					// Heartbeat ACK
					let count = *message_counter.lock().await;
					let now_zoned = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
					let now = strtime::format("%m/%d/%y-%H", &now_zoned).unwrap();
					info!("Heartbeat received. Time: {now}. Since last heartbeat processed: {count} messages");
					*message_counter.lock().await = 0;
				}
				0 => {
					// Dispatch event
					if let Some(d) = &event.d
						&& let Err(e) = handle_message(d, config, &telegram, &last_message_times).await
					{
						error!("Error handling message: {e}");
					}
				}
				_ => {}
			}
		}
	}

	Ok(())
}

async fn handle_message(data: &serde_json::Value, config: &AppConfig, telegram: &TelegramNotifier, last_message_times: &Arc<Mutex<HashMap<String, Timestamp>>>) -> Result<()> {
	let author = data.get("author").and_then(|a| a.get("username")).and_then(|u| u.as_str());
	let content = data.get("content").and_then(|c| c.as_str());
	let channel_id = data.get("channel_id").and_then(|c| c.as_str());

	if let (Some(author), Some(content), Some(channel_id)) = (author, content, channel_id) {
		let is_dm = data.get("guild_id").is_none();
		let now = Timestamp::now();

		let has_ping = content.contains("/ping");
		let is_monitored_user = config.discord.monitored_users.contains(&author.to_string());
		let is_my_message = author == config.discord.my_username;

		// Determine if we should notify for /ping
		if has_ping && !is_my_message {
			let mut should_notify_ping = false;

			if is_dm {
				// In DMs, just /ping is sufficient
				should_notify_ping = true;
			} else {
				// In chats/guilds, need either @mention or reply to my message
				let event_str = serde_json::to_string(data)?;
				let has_mention = event_str.contains(&config.discord.my_username);

				// Check if it's a reply to my message
				let is_reply_to_me = data
					.get("referenced_message")
					.and_then(|m| m.get("author"))
					.and_then(|a| a.get("username"))
					.and_then(|u| u.as_str())
					.map(|u| u == config.discord.my_username)
					.unwrap_or(false);

				if has_mention || is_reply_to_me {
					should_notify_ping = true;
				}
			}

			if should_notify_ping {
				println!("Discord ping from {author}: {content}");
				telegram.send_ping_notification(author, "Discord").await?;
				info!("Successfully sent ping notification for user: {author}");
			}
		}
		// Check for monitored user messages (without /ping)
		else if is_monitored_user && is_dm && !has_ping {
			// Check cooldown: only notify if 15+ minutes have passed since last message
			let mut last_times = last_message_times.lock().await;
			let last_message_time = last_times.get(channel_id).copied();

			let should_notify = if let Some(last_time) = last_message_time {
				let duration_since_last = now.duration_since(last_time);
				// Check if more than 15 minutes have passed
				duration_since_last.as_secs() >= 15 * 60
			} else {
				// No previous message recorded, notify
				true
			};

			if should_notify {
				println!("Discord message from monitored user {author}: {content}");
				telegram.send_monitored_user_message(author, "Discord").await?;
				info!("Successfully sent monitored user notification for: {author}");
			}

			// Update the last message time after checking (for next message)
			last_times.insert(channel_id.to_string(), now);
			drop(last_times);
		} else {
			// For all other messages (including my own), just update the timestamp
			let mut last_times = last_message_times.lock().await;
			last_times.insert(channel_id.to_string(), now);
			drop(last_times);
		}
	}

	Ok(())
}
