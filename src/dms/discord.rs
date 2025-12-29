use std::{collections::HashMap, pin::pin, sync::Arc};

use color_eyre::eyre::{Context, Result};
use futures::future::{Either, select};
use futures_util::{SinkExt, StreamExt, stream::SplitStream};
use jiff::{Timestamp, fmt::strtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
	sync::Mutex,
	time::{self, Duration, Interval},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use tracing::{error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

#[derive(Debug, Deserialize, Serialize)]
struct DiscordMessage {
	op: u8,
	d: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	s: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	t: Option<String>,
}

enum State {
	Disconnected,
	Connected {
		read: SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
		write: Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>>>,
		heartbeat_interval: Interval,
	},
}

pub struct DiscordMonitor {
	config: AppConfig,
	state: State,
	telegram: TelegramNotifier,
	last_message_times: HashMap<String, Timestamp>,
	monitored_users: Vec<String>,
	message_counter: u64,
}

impl DiscordMonitor {
	pub fn new(config: AppConfig) -> Self {
		let telegram = TelegramNotifier::new(config.telegram.clone());
		let monitored_users = config.dms.monitored_users_for_discord();

		Self {
			config,
			state: State::Disconnected,
			telegram,
			last_message_times: HashMap::new(),
			monitored_users,
			message_counter: 0,
		}
	}

	pub async fn collect(&mut self) -> Result<()> {
		match &mut self.state {
			State::Disconnected => {
				// Try to connect
				match self.connect().await {
					Ok((read, write, heartbeat_secs)) => {
						info!("--Discord DM Commands-- connected to WebSocket");
						println!("Discord DM Commands: Connected");
						self.state = State::Connected {
							read,
							write,
							heartbeat_interval: time::interval(Duration::from_secs(heartbeat_secs)),
						};
					}
					Err(e) => {
						error!("Discord connection error: {e}");
						error!("Retrying in 5 minutes...");
						time::sleep(Duration::from_secs(5 * 60)).await;
					}
				}
				Ok(())
			}
			State::Connected { read, write, heartbeat_interval } => {
				enum Event {
					Heartbeat,
					Message(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
				}

				let event = {
					let heartbeat_fut = pin!(heartbeat_interval.tick());
					let msg_fut = pin!(read.next());

					match select(heartbeat_fut, msg_fut).await {
						Either::Left((_tick, _)) => Event::Heartbeat,
						Either::Right((msg, _)) => Event::Message(msg),
					}
				};

				match event {
					Event::Heartbeat => {
						let heartbeat = DiscordMessage {
							op: 1,
							d: Some(json!(null)),
							s: None,
							t: None,
						};
						let msg = serde_json::to_string(&heartbeat)?;
						if write.lock().await.send(Message::Text(msg.into())).await.is_err() {
							error!("Failed to send heartbeat, reconnecting...");
							self.state = State::Disconnected;
						}
					}
					Event::Message(Some(Ok(Message::Text(text)))) => {
						if let Ok(event) = serde_json::from_str::<DiscordMessage>(&text) {
							self.message_counter += 1;

							match event.op {
								11 => {
									// Heartbeat ACK
									let now_zoned = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
									let now = strtime::format("%m/%d/%y-%H", &now_zoned).unwrap();
									info!("Heartbeat received. Time: {now}. Since last heartbeat processed: {} messages", self.message_counter);
									self.message_counter = 0;
								}
								0 => {
									// Dispatch event
									if let Some(d) = &event.d {
										if let Err(e) = self.handle_message(d).await {
											error!("Error handling message: {e}");
										}
									}
								}
								_ => {}
							}
						}
					}
					Event::Message(Some(Ok(_))) => {
						// Non-text message, ignore
					}
					Event::Message(Some(Err(e))) => {
						error!("WebSocket error: {e}, reconnecting...");
						self.state = State::Disconnected;
					}
					Event::Message(None) => {
						error!("WebSocket closed, reconnecting...");
						self.state = State::Disconnected;
					}
				}
				Ok(())
			}
		}
	}

	async fn connect(
		&self,
	) -> Result<(
		SplitStream<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>>,
		Arc<Mutex<futures_util::stream::SplitSink<WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>, Message>>>,
		u64,
	)> {
		let url = "wss://gateway.discord.gg/?v=6&encoding=json";
		let (ws_stream, _) = connect_async(url).await.context("Failed to connect to Discord WebSocket")?;

		let (write, mut read) = ws_stream.split();
		let write = Arc::new(Mutex::new(write));

		// Receive the initial hello message
		let hello_msg = read.next().await.ok_or_else(|| color_eyre::eyre::eyre!("No hello message"))??;
		let hello: DiscordMessage = serde_json::from_str(&hello_msg.to_string())?;

		let heartbeat_interval = hello
			.d
			.as_ref()
			.and_then(|d| d.get("heartbeat_interval"))
			.and_then(|v| v.as_u64())
			.ok_or_else(|| color_eyre::eyre::eyre!("No heartbeat interval"))?;

		let heartbeat_secs = heartbeat_interval / 1000;

		// Send identify payload
		let identify = DiscordMessage {
			op: 2,
			d: Some(json!({
				"token": self.config.dms.discord.user_token,
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

		Ok((read, write, heartbeat_secs))
	}

	async fn handle_message(&mut self, data: &serde_json::Value) -> Result<()> {
		let author = data.get("author").and_then(|a| a.get("username")).and_then(|u| u.as_str());
		let content = data.get("content").and_then(|c| c.as_str());
		let channel_id = data.get("channel_id").and_then(|c| c.as_str());

		if let (Some(author), Some(content), Some(channel_id)) = (author, content, channel_id) {
			let is_dm = data.get("guild_id").is_none();
			let now = Timestamp::now();

			let has_ping = content.contains("/ping");
			let is_monitored_user = self.monitored_users.contains(&author.to_string());
			let is_my_message = author == self.config.dms.discord.my_username;

			// Determine if we should notify for /ping
			if has_ping && !is_my_message {
				let mut should_notify_ping = false;

				if is_dm {
					should_notify_ping = true;
				} else {
					let event_str = serde_json::to_string(data)?;
					let has_mention = event_str.contains(&self.config.dms.discord.my_username);

					let is_reply_to_me = data
						.get("referenced_message")
						.and_then(|m| m.get("author"))
						.and_then(|a| a.get("username"))
						.and_then(|u| u.as_str())
						.map(|u| u == self.config.dms.discord.my_username)
						.unwrap_or(false);

					if has_mention || is_reply_to_me {
						should_notify_ping = true;
					}
				}

				if should_notify_ping {
					println!("Discord ping from {author}: {content}");
					self.telegram.send_ping_notification(author, "Discord").await?;
					info!("Successfully sent ping notification for user: {author}");
				}
			} else if is_monitored_user && is_dm && !has_ping {
				let last_message_time = self.last_message_times.get(channel_id).copied();

				let should_notify = if let Some(last_time) = last_message_time {
					let duration_since_last = now.duration_since(last_time);
					duration_since_last.as_secs() >= 15 * 60
				} else {
					true
				};

				if should_notify {
					println!("Discord message from monitored user {author}: {content}");
					self.telegram.send_monitored_user_message(author, "Discord").await?;
					info!("Successfully sent monitored user notification for: {author}");
				}

				self.last_message_times.insert(channel_id.to_string(), now);
			} else {
				self.last_message_times.insert(channel_id.to_string(), now);
			}
		}

		Ok(())
	}
}
