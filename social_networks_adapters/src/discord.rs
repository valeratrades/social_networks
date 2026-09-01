use std::{convert::Infallible, path::Path, sync::Arc};

use color_eyre::eyre::{Result, WrapErr, eyre};
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
	reach::{Attachment, Author, Direct, Item, Kind, PAGE, Page, Profile, Profiles, Source, Window},
	telegram_notifier::TelegramNotifier,
};

const SURFACE: &str = "discord_dms";
/// A 429 that outlasts this many waits is not a burst.
const RATE_LIMIT_RETRIES: usize = 8;
/// How much of a gap the daemon replays per channel. Bounded by the horizon rather than by paging.
const REPLAY: usize = 50;
const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
/// `MessageType::CALL`, written into the DM the moment a call starts. Replaced the `CALL_CREATE`
/// dispatch, which never produced a notification on this session and named no caller anyway.
const CALL_MESSAGE_TYPE: u64 = 3;
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
	notifier: TelegramNotifier,
	horizon: SignedDuration,
	/// `None` on cold start: a fresh process has no known gap, and backfilling here
	/// would re-beep every deploy.
	last_session_end: Option<Timestamp>,
	disconnects: Vec<Timestamp>,
	rest: Rest,
}

impl DiscordDms {
	pub fn new(discord_config: DiscordConfig, tx: UnboundedSender<DmEvent>, notifier: TelegramNotifier, notification_horizon: Timeframe) -> Self {
		// `Timeframe::signed_duration` did not survive the move out of `v_utils::trades`; the
		// remaining `duration()` is a `std::time::Duration`, and the horizon is subtracted from
		// a jiff `Timestamp`.
		let horizon = SignedDuration::try_from(notification_horizon.duration()).expect("a Timeframe is milliseconds, always in SignedDuration range");
		assert!(horizon > SignedDuration::ZERO, "dms.notification_horizon must be positive");
		Self {
			rest: Rest::new(discord_config.user_token.clone(), discord_config.my_username.clone()),
			discord_config,
			tx,
			message_counter: 0,
			notifier,
			horizon,
			last_session_end: None,
			disconnects: Vec::new(),
		}
	}

	/// Replay DMs that arrived while the gateway was down. Snowflakes encode creation time,
	/// so a cutoff timestamp is the only state needed — no message ids to persist.
	///
	/// `pub` only for `examples/discord_backfill.rs`, which is the sole way to exercise this
	/// path without staging a real disconnect.
	pub async fn backfill(&self, cutoff: Timestamp) -> Result<()> {
		let after = snowflake_at(cutoff);
		for channel in &self.rest.channels().await? {
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

			let messages = self.rest.messages(id, Anchor::After(after), REPLAY).await?;
			info!("Backfilling {} messages from channel {id}", messages.len());
			// newest-first off the wire, and a replay has to arrive in the order it happened
			for (_, message) in messages.iter().rev() {
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

		// Type 3 is the call record, written when the call starts and only updated (never re-created)
		// when it ends, so an unanswered ring lands here too.
		if data.get("type").and_then(|t| t.as_u64()) == Some(CALL_MESSAGE_TYPE) {
			let _ = self.tx.send(DmEvent::IncomingCall {
				platform: "Discord",
				caller: author.to_string(),
			});
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

/// Discord's REST surface: everything that has to be *asked* for, as opposed to what the gateway
/// pushes. The daemon replays a gap through it, and the on-demand axis reads and writes over it.
pub struct Rest {
	http: reqwest::Client,
	token: String,
	my_username: String,
}
impl Rest {
	pub fn new(token: String, my_username: String) -> Self {
		Self {
			http: reqwest::Client::new(),
			token,
			my_username,
		}
	}

	pub async fn channels(&self) -> Result<Vec<serde_json::Value>> {
		self.get("https://discord.com/api/v10/users/@me/channels").await
	}

	/// Only channels that already exist: opening one takes a user id, which nothing outside an open
	/// channel hands us.
	async fn dm_channel(&self, handle: &str) -> Result<(String, String)> {
		self.channels()
			.await?
			.iter()
			.find_map(|c| {
				let channel_id = c.get("id")?.as_str()?;
				let recipient = c.get("recipients")?.as_array()?.iter().find(|r| r.get("username").and_then(|u| u.as_str()) == Some(handle))?;
				Some((channel_id.to_string(), recipient.get("id")?.as_str()?.to_string()))
			})
			.ok_or_else(|| eyre!("no discord DM channel with `{handle}`"))
	}

	async fn messages(&self, channel_id: &str, anchor: Anchor, limit: usize) -> Result<Vec<(u64, serde_json::Value)>> {
		assert!(limit <= PAGE, "discord rejects a limit above {PAGE}");
		let mut query = vec![("limit".to_string(), limit.to_string())];
		match anchor {
			Anchor::Newest => {}
			Anchor::After(id) => query.push(("after".to_string(), id.to_string())),
			Anchor::Before(id) => query.push(("before".to_string(), id.to_string())),
		}
		let page: Vec<serde_json::Value> = self
			.request(|| self.http.get(format!("https://discord.com/api/v10/channels/{channel_id}/messages")).query(&query))
			.await?
			.error_for_status()?
			.json()
			.await?;
		page.into_iter()
			.map(|m| {
				let id = m.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("discord message without an id"))?;
				Ok((id.parse().wrap_err("discord ids are snowflakes")?, m))
			})
			.collect()
	}

	async fn item(&self, channel_id: &str, id: u64, m: &serde_json::Value, assets: &Path) -> Result<Item> {
		let timestamp = m.get("timestamp").and_then(|v| v.as_str()).ok_or_else(|| eyre!("discord message {id} without a timestamp"))?;
		let author = m
			.pointer("/author/username")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("discord message {id} without an author"))?;
		Ok(Item {
			id: id.to_string(),
			source: Source::Discord,
			at: timestamp.parse().wrap_err("discord timestamps are RFC3339")?,
			kind: Kind::Direct,
			author: match author == self.my_username {
				true => Author::Me,
				false => Author::Handle(author.to_string()),
			},
			text: m.get("content").and_then(|v| v.as_str()).unwrap_or_default().to_string(), // an attachment-only message carries no content and is still worth its date
			attachments: self.attachments(m, assets).await?,
			permalink: Some(format!("https://discord.com/channels/@me/{channel_id}/{id}")),
		})
	}

	async fn attachments(&self, m: &serde_json::Value, assets: &Path) -> Result<Vec<Attachment>> {
		let mut out = Vec::new();
		for attachment in m.get("attachments").and_then(|v| v.as_array()).into_iter().flatten() {
			let name = attachment
				.get("filename")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("a discord attachment carries a filename"))?
				.to_string();
			let id = attachment.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a discord attachment carries an id"))?;
			let file = format!("discord-{id}.avif");
			if assets.join(&file).exists() {
				out.push(Attachment::Image { file });
				continue;
			}
			// an undeclared content type is one discord itself would not call an image
			let mime = attachment.get("content_type").and_then(|v| v.as_str()).unwrap_or_default();
			if !mime.starts_with("image/") {
				out.push(Attachment::File { name });
				continue;
			}

			let url = attachment.get("url").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a discord attachment carries a url"))?;
			// the cdn url is signed; the token belongs on the api, not on it
			let bytes = self.http.get(url).send().await?.error_for_status()?.bytes().await?;
			out.push(Attachment::keep(&bytes, mime, name, assets, file));
		}
		Ok(out)
	}

	/// Discord answers a burst with a 429 and a body saying how long to hold off. A backfill is
	/// hundreds of requests, so meeting one is expected rather than exceptional.
	async fn request(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
		for _ in 0..RATE_LIMIT_RETRIES {
			// user tokens take no `Bot ` prefix
			let response = build().header("authorization", &self.token).send().await?;
			if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
				return Ok(response);
			}
			let body: serde_json::Value = response.json().await?;
			let after = body
				.get("retry_after")
				.and_then(|v| v.as_f64())
				.ok_or_else(|| eyre!("a discord 429 carries `retry_after`: {body}"))?;
			warn!("discord: rate limited, holding off {after}s");
			time::sleep(Duration::from_secs_f64(after)).await;
		}
		Err(eyre!("discord: still rate limited after {RATE_LIMIT_RETRIES} waits"))
	}

	async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
		Ok(self.request(|| self.http.get(url)).await?.error_for_status()?.json().await?)
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

impl Profiles for Rest {
	/// Discord publishes no feed a person's own activity could be read off, so the window bounds
	/// nothing here.
	async fn profile(&mut self, handle: &str, _window: Window) -> Result<Profile> {
		let (_, user_id) = self.dm_channel(handle).await?;
		let mut profile = Profile::default();

		// 404 here means no note is set, not that the user is gone.
		let note = self.request(|| self.http.get(format!("https://discord.com/api/v10/users/@me/notes/{user_id}"))).await?;
		if note.status() != reqwest::StatusCode::NOT_FOUND {
			let note: serde_json::Value = note.error_for_status()?.json().await?;
			profile.state("discord:note", note.get("note").and_then(|v| v.as_str()));
		}

		let payload: serde_json::Value = self.get(&format!("https://discord.com/api/v10/users/{user_id}/profile")).await?;
		// `/user_profile/bio` carries the same text; it only diverges per-guild, which `@me` never is
		profile.state("discord:bio", payload.pointer("/user/bio").and_then(|v| v.as_str()));
		profile.state("discord:pronouns", payload.pointer("/user_profile/pronouns").and_then(|v| v.as_str()));
		profile.display = payload.pointer("/user/global_name").and_then(|v| v.as_str()).map(str::to_string);
		for account in payload.get("connected_accounts").and_then(|v| v.as_array()).into_iter().flatten() {
			if let (Some(kind), Some(name)) = (account.get("type").and_then(|v| v.as_str()), account.get("name").and_then(|v| v.as_str())) {
				profile.handles.insert(kind.to_string(), name.to_string());
			}
		}
		Ok(profile)
	}
}

impl Direct for Rest {
	async fn direct(&mut self, handle: &str, window: Window, assets: &Path) -> Result<Page> {
		let (channel_id, _) = self.dm_channel(handle).await?;
		let limit = window.limit();
		let mut raw: Vec<(u64, serde_json::Value)> = Vec::new();
		let mut exhausted = false;

		match &window {
			// one page under the floor, checked in by the caller before it asks for the next
			Window::Below { before, .. } => {
				let anchor = match before {
					Some(floor) => Anchor::Before(floor.parse().wrap_err("a discord backfill floor is a snowflake")?),
					None => Anchor::Newest,
				};
				let page = self.messages(&channel_id, anchor, PAGE.min(limit)).await?;
				exhausted = page.len() < PAGE.min(limit);
				raw = page;
			}
			// forward from the checkpoint: `after=<id>` returns the oldest window past it
			Window::Above { after: Some(after), .. } => {
				let mut anchor = Anchor::After(after.parse().wrap_err("a discord checkpoint is a snowflake")?);
				//LOOP: walks strictly upwards from a fixed checkpoint towards the newest message, and
				// stops on the first page that is not full
				loop {
					let page = self.messages(&channel_id, anchor, PAGE).await?;
					let ids: Vec<u64> = page.iter().map(|(id, _)| *id).collect();
					raw.extend(page);
					if raw.len() >= limit {
						warn!("discord `{handle}`: stopping at {limit} messages, the rest comes on the next pull");
						break;
					}
					match next_after(&ids, PAGE) {
						Some(next) => anchor = Anchor::After(next),
						None => break,
					}
				}
			}
			// walk backwards from the newest, since there is no floor to walk up from
			Window::Above { after: None, .. } => {
				let mut anchor = Anchor::Newest;
				//LOOP: bounded by `limit` and by the conversation, which is walked strictly downwards
				while raw.len() < limit {
					let page = self.messages(&channel_id, anchor, PAGE.min(limit - raw.len())).await?;
					let Some(oldest) = page.iter().map(|(id, _)| *id).min() else {
						exhausted = true;
						break;
					};
					let short = page.len() < PAGE;
					raw.extend(page);
					if short {
						exhausted = true;
						break;
					}
					anchor = Anchor::Before(oldest);
				}
			}
		}

		let mut items = Vec::with_capacity(raw.len());
		for (id, m) in &raw {
			items.push(self.item(&channel_id, *id, m, assets).await?);
		}
		items.retain(|item| !window.reached(item.at));
		items.sort_by_key(|item| item.at);
		Ok(Page {
			newest: raw.iter().map(|(id, _)| *id).max().map(|id| id.to_string()),
			oldest: raw.iter().map(|(id, _)| *id).min().map(|id| id.to_string()),
			exhausted,
			items,
		})
	}

	async fn send(&mut self, handle: &str, text: &str) -> Result<()> {
		let (channel_id, _) = self.dm_channel(handle).await?;
		self.request(|| {
			self.http
				.post(format!("https://discord.com/api/v10/channels/{channel_id}/messages"))
				.json(&serde_json::json!({ "content": text }))
		})
		.await?
		.error_for_status()?;
		Ok(())
	}
}

#[derive(Clone, Copy)]
enum Anchor {
	Newest,
	After(u64),
	Before(u64),
}

/// `after=<id>` returns the oldest window past the cursor, newest-first inside it — so the next
/// cursor is the page's largest id, and a short page is the last one.
fn next_after(ids: &[u64], limit: usize) -> Option<u64> {
	(ids.len() == limit).then(|| ids.iter().copied().max().expect("a full page is non-empty"))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cursor_walks_forward() {
		assert_eq!(next_after(&[30, 20, 10], 3), Some(30));
		assert_eq!(next_after(&[20, 10], 3), None);
		assert_eq!(next_after(&[], 3), None);
	}

	#[test]
	fn snowflake_matches_discord() {
		// snowflake 1367859374448050318 encodes 2025-05-02T13:44:48.466Z
		let ts: Timestamp = "2025-05-02T13:44:48.466Z".parse().unwrap();
		assert_eq!(snowflake_at(ts), 1367859374448050318u64 & !((1 << 22) - 1));
	}

	/// Payload trimmed off a real incoming call (`GET /channels/{id}/messages`), which is the same
	/// object the gateway hands to `MESSAGE_CREATE`.
	#[test]
	fn call_message_is_an_incoming_call() {
		let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
		let config = DiscordConfig {
			user_token: String::new(),
			my_username: "valeratrades".to_string(),
		};
		let discord = DiscordDms::new(
			config,
			tx,
			TelegramNotifier::new(Default::default()),
			Timeframe::from_naive(1, v_utils::TimeframeDesignator::Hours),
		);

		let call = serde_json::json!({
			"type": 3,
			"channel_id": "1446523722717728891",
			"author": {"id": "1442413014119743508", "username": "p056212"},
			"content": "",
			"call": {"participants": ["474661840735961089"]},
		});
		discord.handle_message(&call).unwrap();
		assert!(matches!(rx.try_recv().unwrap(), DmEvent::IncomingCall { platform: "Discord", caller } if caller == "p056212"));

		// our own outgoing call leaves an identical record, authored by us
		let mine = serde_json::json!({
			"type": 3,
			"channel_id": "1446523722717728891",
			"author": {"id": "474661840735961089", "username": "valeratrades"},
			"content": "",
		});
		discord.handle_message(&mine).unwrap();
		assert!(rx.try_recv().is_err());
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
