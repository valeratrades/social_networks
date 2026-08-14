use std::{convert::Infallible, sync::Arc};

use color_eyre::eyre::Result;
use futures::future::{Either, select};
use futures_util::{SinkExt, StreamExt, stream::SplitStream};
use jiff::{SignedDuration, Timestamp, fmt::strtime};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::{
	sync::{Mutex, mpsc::UnboundedSender},
	time::{self, Duration},
};
use tokio_tungstenite::{
	MaybeTlsStream, WebSocketStream, connect_async,
	tungstenite::{Message, protocol::frame::coding::CloseCode},
};
use tracing::{error, info, warn};
use v_utils::{Timeframe, macros::MyConfigPrimitives};

use crate::{
	client::{AdapterError, Client},
	dm_event::DmEvent,
	telegram_notifier::TelegramNotifier,
};

const SURFACE: &str = "discord_dms";
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
/// A session shorter than this counts as a failed connect, so backoff keeps growing.
const STABLE_SESSION: SignedDuration = SignedDuration::from_secs(60);
const FLAP_WINDOW: SignedDuration = SignedDuration::from_hours(1);
const FLAP_THRESHOLD: usize = 5;

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct DiscordConfig {
	pub user_token: String,
	pub my_username: String,
}

pub struct DiscordDms {
	discord_config: DiscordConfig,
	tx: UnboundedSender<DmEvent>,
	message_counter: u64,
	my_user_id: Option<String>,
	notifier: TelegramNotifier,
	horizon: SignedDuration,
	/// `None` on cold start: a fresh process has no known gap, and backfilling here
	/// would re-beep every deploy.
	last_session_end: Option<Timestamp>,
	disconnects: Vec<Timestamp>,
	http: reqwest::Client,
}

impl DiscordDms {
	pub fn new(discord_config: DiscordConfig, tx: UnboundedSender<DmEvent>, notifier: TelegramNotifier, notification_horizon: Timeframe) -> Self {
		// `Timeframe::signed_duration` did not survive the move out of `v_utils::trades`; the
		// remaining `duration()` is a `std::time::Duration`, and the horizon is subtracted from
		// a jiff `Timestamp`.
		let horizon = SignedDuration::try_from(notification_horizon.duration()).expect("a Timeframe is milliseconds, always in SignedDuration range");
		assert!(horizon > SignedDuration::ZERO, "dms.notification_horizon must be positive");
		Self {
			discord_config,
			tx,
			message_counter: 0,
			my_user_id: None,
			notifier,
			horizon,
			last_session_end: None,
			disconnects: Vec::new(),
			http: reqwest::Client::new(),
		}
	}

	/// Replay DMs that arrived while the gateway was down. Snowflakes encode creation time,
	/// so a cutoff timestamp is the only state needed — no message ids to persist.
	///
	/// `pub` only for `examples/discord_backfill.rs`, which is the sole way to exercise this
	/// path without staging a real disconnect.
	pub async fn backfill(&self, cutoff: Timestamp) -> Result<()> {
		let after = snowflake_at(cutoff);
		let channels: Vec<serde_json::Value> = self
			.http
			.get("https://discord.com/api/v10/users/@me/channels")
			// user tokens take no `Bot ` prefix
			.header("authorization", &self.discord_config.user_token)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?;

		for channel in &channels {
			let Some(id) = channel.get("id").and_then(|v| v.as_str()) else {
				continue;
			};
			// free filter off the channel list: idle DMs cost zero extra requests
			let last_message = channel
				.get("last_message_id")
				.and_then(|v| v.as_str())
				.map(|s| s.parse::<u64>().expect("discord snowflakes are numeric strings"));
			if last_message.is_none_or(|l| l <= after) {
				continue;
			}

			let messages: Vec<serde_json::Value> = self
				.http
				.get(format!("https://discord.com/api/v10/channels/{id}/messages"))
				.query(&[("limit", "50".to_string()), ("after", after.to_string())])
				.header("authorization", &self.discord_config.user_token)
				.send()
				.await?
				.error_for_status()?
				.json()
				.await?;

			info!("Backfilling {} messages from channel {id}", messages.len());
			for message in messages.iter().rev() {
				self.handle_message(message)?;
			}
		}
		Ok(())
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

		if let Some(last_end) = self.last_session_end {
			let cutoff = backfill_cutoff(last_end, Timestamp::now(), self.horizon);
			if let Err(e) = self.backfill(cutoff).await {
				error!("Discord backfill failed: {e:#}");
			}
		}

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
										Some("CALL_CREATE") => self.handle_call_create(d),
										// Only MESSAGE_CREATE: Discord also fires MESSAGE_UPDATE with identical content
										// when it unfurls links/embeds, which would double-notify.
										Some("MESSAGE_CREATE") => self.handle_message(d),
										_ => Ok(()),
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
				"token": self.discord_config.user_token,
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

	fn handle_call_create(&self, data: &serde_json::Value) -> Result<()> {
		let my_id = self.my_user_id.as_deref().ok_or_else(|| color_eyre::eyre::eyre!("my_user_id not set"))?;
		let ringing = data.get("ringing").and_then(|r| r.as_array());
		if let Some(ringing) = ringing
			&& ringing.iter().any(|id| id.as_str() == Some(my_id))
		{
			// CALL_CREATE names no caller; the DM channel is one-to-one, so it identifies them.
			let channel_id = data
				.get("channel_id")
				.and_then(|c| c.as_str())
				.ok_or_else(|| color_eyre::eyre::eyre!("CALL_CREATE missing channel_id"))?;
			let _ = self.tx.send(DmEvent::IncomingCall {
				platform: "Discord",
				caller: channel_id.to_string(),
			});
		}
		Ok(())
	}

	fn handle_message(&self, data: &serde_json::Value) -> Result<()> {
		let author = data.get("author").and_then(|a| a.get("username")).and_then(|u| u.as_str());
		let content = data.get("content").and_then(|c| c.as_str());
		let channel_id = data.get("channel_id").and_then(|c| c.as_str());

		let (Some(author), Some(content), Some(channel_id)) = (author, content, channel_id) else {
			return Ok(());
		};

		// Discord WS echoes our own outgoing messages back to us; drop those at the transport boundary,
		// mirroring Telegram's `!message.outgoing()` filter.
		if author == self.discord_config.my_username {
			return Ok(());
		}

		let is_dm = data.get("guild_id").is_none();

		// Mention detection: Discord's payload has structured `mentions` arrays, but the historical
		// implementation used a substring scan of the whole JSON (catches role mentions, raw `@name`,
		// etc.). Preserve that behavior.
		let mentions_me = if is_dm {
			false
		} else {
			let event_str = serde_json::to_string(data)?;
			event_str.contains(&self.discord_config.my_username)
		};

		let is_reply_to_me = data
			.get("referenced_message")
			.and_then(|m| m.get("author"))
			.and_then(|a| a.get("username"))
			.and_then(|u| u.as_str())
			.map(|u| u == self.discord_config.my_username)
			.unwrap_or(false);

		let _ = self.tx.send(DmEvent::Message {
			platform: "Discord",
			sender: author.to_string(),
			text: content.to_string(),
			chat_id: channel_id.to_string(),
			is_dm,
			mentions_me,
			is_reply_to_me,
		});

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
			let started = Timestamp::now();
			let outcome = self.run_session().await;
			let ended = Timestamp::now();
			self.last_session_end = Some(ended);
			outcome?;

			if ended.duration_since(started) > STABLE_SESSION {
				attempt = 0;
			}

			self.disconnects.push(ended);
			self.disconnects.retain(|t| ended.duration_since(*t) < FLAP_WINDOW);
			if self.disconnects.len() >= FLAP_THRESHOLD {
				let detail = format!("{} disconnects within the last hour", self.disconnects.len());
				self.notifier.report_recoverable(SURFACE, &detail).await;
				self.disconnects.clear();
			}

			let delay = reconnect_delay(attempt);
			warn!("Discord reconnecting in {:.1}s (attempt {attempt})", delay.as_secs_f64());
			time::sleep(delay).await;
			attempt = attempt.saturating_add(1);
		}
	}
}

fn snowflake_at(ts: Timestamp) -> u64 {
	let ms = ts.as_millisecond() - DISCORD_EPOCH_MS;
	assert!(ms > 0, "cutoff predates Discord epoch");
	(ms as u64) << 22
}

/// Bounded by the horizon so a multi-day outage doesn't bombard on reconnect.
fn backfill_cutoff(last_session_end: Timestamp, now: Timestamp, horizon: SignedDuration) -> Timestamp {
	last_session_end.max(now - horizon)
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn snowflake_matches_discord() {
		// snowflake 1367859374448050318 encodes 2025-05-02T13:44:48.466Z
		let ts: Timestamp = "2025-05-02T13:44:48.466Z".parse().unwrap();
		assert_eq!(snowflake_at(ts), 1367859374448050318u64 & !((1 << 22) - 1));
	}

	#[test]
	fn cutoff_clamps_to_horizon() {
		let now: Timestamp = "2025-05-02T12:00:00Z".parse().unwrap();
		let horizon = SignedDuration::from_hours(12);
		let long_outage: Timestamp = "2025-04-30T12:00:00Z".parse().unwrap();
		assert_eq!(backfill_cutoff(long_outage, now, horizon), now - horizon);
		let blip = now - SignedDuration::from_mins(2);
		assert_eq!(backfill_cutoff(blip, now, horizon), blip);
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
