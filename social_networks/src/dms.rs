use std::collections::HashMap;

use clap::Args;
use jiff::Timestamp;
use serde::Deserialize;
use social_networks_adapters::{DmEvent, discord::DiscordConfig, telegram_notifier::TelegramNotifier};
use tracing::{error, info};
use v_utils::macros::MyConfigPrimitives;

const MONITORED_USER_THROTTLE_SECS: i64 = 15 * 60;
/// CLI args for the `dms` subcommand. Empty today, kept as a placeholder so the
/// subcommand can grow flags without changing the command surface.
#[derive(Args)]
pub struct DmsArgs {}

/// Configuration for DM monitoring (ping, monitored users) across Discord and Telegram.
///
/// Lives in the binary, not the adapters: deciding whether to notify is application logic,
/// not transport.
#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct DmsConfig {
	/// Users to monitor across all platforms. Can be either:
	/// - A plain string (applies to all platforms)
	/// - An object like {telegram = "username"} or {discord = "username"}
	#[serde(default)]
	#[primitives(skip)]
	pub monitored_users: Vec<MonitoredUser>,
	#[serde(default)]
	#[primitives(skip)]
	pub discord: DiscordConfig,
}

impl DmsConfig {
	fn monitored_matches(&self, platform: &str, username: &str) -> bool {
		self.monitored_users.iter().any(|u| match u {
			MonitoredUser::All(u) => u == username,
			MonitoredUser::Discord(u) => platform == "Discord" && u == username,
			MonitoredUser::Telegram(u) => platform == "Telegram" && u == username,
		})
	}
}

/// A monitored user can be either global (all platforms) or platform-specific.
#[derive(Clone, Debug)]
pub enum MonitoredUser {
	All(String),
	Discord(String),
	Telegram(String),
}

impl<'de> Deserialize<'de> for MonitoredUser {
	fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>, {
		use serde::de::{MapAccess, Visitor};

		struct V;

		impl<'de> Visitor<'de> for V {
			type Value = MonitoredUser;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a string or an object with 'telegram' or 'discord' key")
			}

			fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E>
			where
				E: serde::de::Error, {
				Ok(MonitoredUser::All(v.to_string()))
			}

			fn visit_map<M>(self, mut map: M) -> std::result::Result<Self::Value, M::Error>
			where
				M: MapAccess<'de>, {
				let key: String = map.next_key()?.ok_or_else(|| serde::de::Error::custom("expected a key"))?;
				let value: String = map.next_value()?;
				match key.as_str() {
					"telegram" => Ok(MonitoredUser::Telegram(value)),
					"discord" => Ok(MonitoredUser::Discord(value)),
					other => Err(serde::de::Error::custom(format!("unknown platform: {other}"))),
				}
			}
		}

		deserializer.deserialize_any(V)
	}
}

impl serde::Serialize for MonitoredUser {
	fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
	where
		S: serde::Serializer, {
		use serde::ser::SerializeMap as _;

		match self {
			MonitoredUser::All(u) => serializer.serialize_str(u),
			MonitoredUser::Discord(u) => {
				let mut map = serializer.serialize_map(Some(1))?;
				map.serialize_entry("discord", u)?;
				map.end()
			}
			MonitoredUser::Telegram(u) => {
				let mut map = serializer.serialize_map(Some(1))?;
				map.serialize_entry("telegram", u)?;
				map.end()
			}
		}
	}
}

/// Consume DM events forever, applying notification rules. Returns when the event
/// stream is closed (both adapters dropped their senders), which only happens on
/// shutdown.
pub async fn run(mut events: tokio::sync::mpsc::UnboundedReceiver<DmEvent>, config: DmsConfig, notifier: TelegramNotifier) {
	// Throttle map for monitored-user notifications, keyed by (platform, chat_id).
	let mut last_seen: HashMap<(&'static str, String), Timestamp> = HashMap::new();

	while let Some(event) = events.recv().await {
		match event {
			DmEvent::IncomingCall { platform } => {
				println!("Incoming call on {platform}");
				if let Err(e) = notifier.send_call_notification(platform).await {
					error!("Error sending call notification: {e}");
				} else {
					info!("Successfully sent call notification ({platform})");
				}
			}
			DmEvent::Message {
				platform,
				sender,
				text,
				chat_id,
				is_dm,
				mentions_me,
				is_reply_to_me,
			} => {
				let has_ping = text.contains("/ping");
				let addressed_to_me = is_dm || mentions_me || is_reply_to_me;

				if has_ping && addressed_to_me {
					println!("{platform} ping from {sender}: {text}");
					if let Err(e) = notifier.send_ping_notification(&sender, platform).await {
						error!("Error sending ping notification: {e}");
					} else {
						info!("Successfully sent ping notification for user: {sender}");
					}
					continue;
				}

				if is_dm && !has_ping && config.monitored_matches(platform, &sender) {
					let now = Timestamp::now();
					let key = (platform, chat_id);
					let should_notify = match last_seen.get(&key) {
						None => true,
						Some(prev) => now.duration_since(*prev).as_secs() >= MONITORED_USER_THROTTLE_SECS,
					};
					if should_notify {
						println!("{platform} message from monitored user {sender}: {text}");
						if let Err(e) = notifier.send_monitored_user_message(&sender, platform).await {
							error!("Error sending monitored user notification: {e}");
						} else {
							info!("Successfully sent monitored user notification for: {sender}");
						}
					}
					last_seen.insert(key, now);
				}
			}
		}
	}
}
