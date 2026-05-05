use std::{collections::HashMap, convert::Infallible, sync::Arc};

use clap::Args;
use color_eyre::eyre::Result;
use futures::future::{Either, select};
use futures_util::{SinkExt, StreamExt, stream::SplitStream};
use jiff::{Timestamp, fmt::strtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use social_networks_utils::{config::AppConfig, telegram_notifier::TelegramNotifier};
use tokio::{
	sync::Mutex,
	time::{self, Duration},
};
use tokio_tungstenite::{
	MaybeTlsStream, WebSocketStream, connect_async,
	tungstenite::{Message, protocol::frame::coding::CloseCode},
};
use tracing::{error, info, warn};

use crate::client::{AdapterError, Client};

const SURFACE: &str = "discord_dms";
#[derive(Args)]
pub struct DmsArgs {}

pub struct DiscordDms {
	config: AppConfig,
	telegram: TelegramNotifier,
	last_message_times: HashMap<String, Timestamp>,
	monitored_users: Vec<String>,
	message_counter: u64,
	my_user_id: Option<String>,
}

impl DiscordDms {
	pub fn new(config: AppConfig) -> Self {
		let telegram = TelegramNotifier::new(config.telegram.clone());
		let monitored_users = config.dms.monitored_users_for_discord();

		Self {
			config,
			telegram,
			last_message_times: HashMap::new(),
			monitored_users,
			message_counter: 0,
			my_user_id: None,
		}
	}

	/// Run one connection lifetime: connect, then loop until the WS dies.
	/// Returns `Ok(())` if the caller should reconnect, `Err(AdapterError::Auth)` if
	/// retrying cannot help (datacenter banned, token revoked, etc.).
	async fn run_session(&mut self) -> Result<(), AdapterError> {
		let (mut read, write, heartbeat_secs) = match self.connect().await {
			Ok(c) => c,
			Err(e) => {
				error!("Discord connection error: {e:#}");
				return Ok(());
			}
		};
		info!("--Discord DM Commands-- connected to WebSocket");
		println!("Discord DM Commands: Connected");

		let mut heartbeat_interval = time::interval(Duration::from_secs(heartbeat_secs));

		loop {
			enum Event {
				Heartbeat,
				Message(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
			}

			let event = {
				let heartbeat_fut = std::pin::pin!(heartbeat_interval.tick());
				let msg_fut = std::pin::pin!(read.next());

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
					let msg = match serde_json::to_string(&heartbeat) {
						Ok(m) => m,
						Err(e) =>
							return Err(AdapterError::Unhandled {
								surface: SURFACE,
								detail: format!("heartbeat serialization: {e}"),
							}),
					};
					if write.lock().await.send(Message::Text(msg.into())).await.is_err() {
						error!("Failed to send Discord heartbeat, reconnecting...");
						return Ok(());
					}
				}
				Event::Message(Some(Ok(Message::Text(text)))) =>
					if let Ok(event) = serde_json::from_str::<DiscordMessage>(&text) {
						self.message_counter += 1;

						match event.op {
							11 => {
								let now_zoned = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
								let now = strtime::format("%m/%d/%y-%H", &now_zoned).unwrap();
								info!("Heartbeat received. Time: {now}. Since last heartbeat processed: {} messages", self.message_counter);
								self.message_counter = 0;
							}
							0 =>
								if let Some(d) = &event.d {
									let event_type = event.t.as_deref();
									let result = match event_type {
										Some("READY") => self.handle_ready(d),
										Some("CALL_CREATE") => self.handle_call_create(d).await,
										_ => self.handle_message(d).await,
									};
									if let Err(e) = result {
										error!("Error handling {}: {e}", event_type.unwrap_or("unknown"));
									}
								},
							_ => {}
						}
					},
				Event::Message(Some(Ok(Message::Close(frame)))) => {
					return classify_close(frame);
				}
				Event::Message(Some(Ok(_))) => {
					// Non-text non-close message (Ping/Pong/Binary), ignore
				}
				Event::Message(Some(Err(e))) => {
					error!("Discord WebSocket error: {e}, reconnecting...");
					return Ok(());
				}
				Event::Message(None) => {
					error!("Discord WebSocket closed (no frame), reconnecting...");
					return Ok(());
				}
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
		let url = "wss://gateway.discord.gg/?v=10&encoding=json";
		let (ws_stream, _) = connect_async(url).await?;

		let (write, mut read) = ws_stream.split();
		let write = Arc::new(Mutex::new(write));

		let hello_msg = read.next().await.ok_or_else(|| color_eyre::eyre::eyre!("No hello message"))??;
		let hello: DiscordMessage = serde_json::from_str(&hello_msg.to_string())?;

		let heartbeat_interval = hello
			.d
			.as_ref()
			.and_then(|d| d.get("heartbeat_interval"))
			.and_then(|v| v.as_u64())
			.ok_or_else(|| color_eyre::eyre::eyre!("No heartbeat interval"))?;

		let heartbeat_secs = heartbeat_interval / 1000;

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

	fn handle_ready(&mut self, data: &serde_json::Value) -> Result<()> {
		let user_id = data
			.get("user")
			.and_then(|u| u.get("id"))
			.and_then(|id| id.as_str())
			.ok_or_else(|| color_eyre::eyre::eyre!("READY event missing user.id"))?;
		info!("Captured my user id: {user_id}");
		self.my_user_id = Some(user_id.to_string());
		Ok(())
	}

	async fn handle_call_create(&self, data: &serde_json::Value) -> Result<()> {
		let my_id = self.my_user_id.as_deref().ok_or_else(|| color_eyre::eyre::eyre!("my_user_id not set"))?;
		let ringing = data.get("ringing").and_then(|r| r.as_array());
		if let Some(ringing) = ringing
			&& ringing.iter().any(|id| id.as_str() == Some(my_id))
		{
			let channel_id = data.get("channel_id").and_then(|c| c.as_str()).unwrap_or("unknown");
			println!("Incoming Discord call on channel {channel_id}");
			self.telegram.send_call_notification("Discord").await?;
			info!("Sent call notification for channel: {channel_id}");
		}
		Ok(())
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

impl Client for DiscordDms {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		let mut attempt: u32 = 0;
		loop {
			match self.run_session().await {
				Ok(()) => {
					let delay = reconnect_delay(attempt);
					warn!("Discord reconnecting in {:.1}s (attempt {attempt})", delay.as_secs_f64());
					time::sleep(delay).await;
					attempt = attempt.saturating_add(1);
				}
				Err(e) => return Err(e),
			}
		}
	}
}

fn reconnect_delay(attempt: u32) -> Duration {
	let delay_secs = std::f64::consts::E.powi(attempt as i32).min(600.0);
	Duration::from_secs_f64(delay_secs)
}

/// Map a Discord WS close frame to either a recoverable reconnect (`Ok(())`) or a fatal
/// auth-class error. Codes 4004/4010-4014 are documented as fatal in the Discord
/// gateway docs (invalid token, invalid intent, datacenter blocked, etc.).
fn classify_close(frame: Option<tokio_tungstenite::tungstenite::protocol::frame::CloseFrame>) -> Result<(), AdapterError> {
	let Some(frame) = frame else {
		error!("Discord WS closed with no frame, reconnecting...");
		return Ok(());
	};
	let code: u16 = match frame.code {
		CloseCode::Library(n) => n,
		other => u16::from(other),
	};
	match code {
		4004 | 4010 | 4011 | 4012 | 4013 | 4014 => Err(AdapterError::Auth {
			surface: SURFACE,
			detail: format!("Discord WS close code {code}: {}", frame.reason),
		}),
		_ => {
			error!("Discord WS closed with code {code}: {}, reconnecting...", frame.reason);
			Ok(())
		}
	}
}

#[derive(Debug, Deserialize, Serialize)]
struct DiscordMessage {
	op: u8,
	d: Option<serde_json::Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	s: Option<u64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	t: Option<String>,
}
