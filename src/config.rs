use serde::Deserialize;
pub use tg::TelegramDestination;
use v_utils::{
	macros::{LiveSettings, MyConfigPrimitives, Settings},
	trades::Timeframe,
};

#[derive(Clone, Debug, Default, LiveSettings, MyConfigPrimitives, Settings)]
pub struct AppConfig {
	#[settings(skip)]
	#[serde(default)]
	pub dms: DmsConfig,
	#[settings(skip)]
	#[serde(default)]
	pub telegram: TelegramConfig,
	#[settings(skip)]
	#[serde(default)]
	pub twitter: TwitterConfig,
	#[settings(skip)]
	#[serde(default)]
	pub youtube: YoutubeConfig,
	#[settings(skip)]
	#[serde(default)]
	pub email: Option<EmailConfig>,
	#[settings(skip)]
	#[serde(default)]
	pub clickhouse: ClickHouseConfig,
}

/// Configuration for DM monitoring (ping, monitored users)
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
	/// Get list of usernames to monitor for Discord
	pub fn monitored_users_for_discord(&self) -> Vec<String> {
		self.monitored_users
			.iter()
			.filter_map(|u| match u {
				MonitoredUser::All(username) => Some(username.clone()),
				MonitoredUser::Discord(username) => Some(username.clone()),
				MonitoredUser::Telegram(_) => None,
			})
			.collect()
	}

	/// Get list of usernames to monitor for Telegram
	pub fn monitored_users_for_telegram(&self) -> Vec<String> {
		self.monitored_users
			.iter()
			.filter_map(|u| match u {
				MonitoredUser::All(username) => Some(username.clone()),
				MonitoredUser::Telegram(username) => Some(username.clone()),
				MonitoredUser::Discord(_) => None,
			})
			.collect()
	}
}

/// A monitored user can be either global (all platforms) or platform-specific
#[derive(Clone, Debug)]
pub enum MonitoredUser {
	/// Applies to all platforms
	All(String),
	/// Discord-specific
	Discord(String),
	/// Telegram-specific
	Telegram(String),
}

impl<'de> Deserialize<'de> for MonitoredUser {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: serde::Deserializer<'de>, {
		use serde::de::{MapAccess, Visitor};

		struct MonitoredUserVisitor;

		impl<'de> Visitor<'de> for MonitoredUserVisitor {
			type Value = MonitoredUser;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a string or an object with 'telegram' or 'discord' key")
			}

			fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
			where
				E: serde::de::Error, {
				Ok(MonitoredUser::All(v.to_string()))
			}

			fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
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

		deserializer.deserialize_any(MonitoredUserVisitor)
	}
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct DiscordConfig {
	pub user_token: String,
	pub my_username: String,
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct TelegramConfig {
	pub bot_token: String,
	#[private_value]
	pub channel_alerts: TelegramDestination,
	#[private_value]
	pub channel_output: TelegramDestination,
	pub api_id: i32,
	pub api_hash: String,
	pub phone: String,
	pub username: String,
	#[primitives(skip)]
	pub poll_channels: Vec<String>,
	#[primitives(skip)]
	pub info_channels: Vec<String>,
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct TwitterConfig {
	pub bearer_token: String,
	pub everytime_polls_list: String,
	pub sometimes_polls_list: String,
	#[primitives(skip)]
	pub oauth: Option<TwitterOauthConfig>,
	#[primitives(skip)]
	pub poll: Option<TwitterPollConfig>,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct TwitterOauthConfig {
	pub acc_username: String,
	pub api_key: String,
	pub api_key_secret: String,
	pub access_token: String,
	pub access_token_secret: String,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct TwitterPollConfig {
	pub text: String,
	pub duration_hours: u32,
	pub schedule_every: Timeframe,
	#[serde(default = "__default_num_of_retries")]
	pub num_of_retries: u8,
}
fn __default_num_of_retries() -> u8 {
	3
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct YoutubeConfig {
	#[primitives(skip)]
	pub channels: std::collections::HashMap<String, String>,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct EmailConfig {
	/// Gmail email address to monitor
	pub email: String,
	/// Authentication method (IMAP or OAuth)
	#[primitives(skip)]
	pub auth: EmailAuth,
	/// Regex patterns to match against sender email to ignore (skip processing entirely)
	#[serde(default)]
	#[primitives(skip)]
	pub ignore_patterns: Vec<String>,
	/// Patterns that mark an email as alert-worthy without LLM evaluation
	#[serde(default)]
	#[primitives(skip)]
	pub important_if_contains: ImportantIfContains,
	/// Claude API token for LLM-based email classification (optional, falls back to CLAUDE_TOKEN env var)
	#[serde(default)]
	pub claude_token: Option<String>,
}

/// Patterns to check for marking email as alert-worthy.
/// If any pattern matches, the email is forwarded without LLM check.
/// Top-level `any` matches against all fields (subject, body, address).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ImportantIfContains {
	/// Patterns to match against any field (subject, body, address)
	#[serde(default)]
	pub any: Vec<String>,
	/// Patterns to match against subject/title only
	#[serde(default)]
	pub subject: Vec<String>,
	/// Patterns to match against body only
	#[serde(default)]
	pub body: Vec<String>,
	/// Patterns to match against sender address only
	#[serde(default)]
	pub address: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailAuth {
	Imap(ImapAuth),
	Oauth(OAuthAuth),
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct ImapAuth {
	pub pass: String,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct OAuthAuth {
	pub client_id: String,
	pub client_secret: String,
	/// Path to store auth tokens (default: ~/.local/state/social_networks/gmail_tokens.json)
	#[serde(default = "__default_email_token_path")]
	#[primitives(skip)]
	pub token_path: String,
}

fn __default_email_token_path() -> String {
	let app_name = env!("CARGO_PKG_NAME");
	let xdg_dirs = xdg::BaseDirectories::with_prefix(app_name);
	xdg_dirs.place_state_file("gmail_tokens.json").unwrap().display().to_string()
}

#[derive(Clone, Debug, Deserialize)]
pub struct ClickHouseConfig {
	#[serde(default = "__default_clickhouse_url")]
	pub url: String,
	#[serde(default = "__default_clickhouse_database")]
	pub database: String,
	#[serde(default = "__default_clickhouse_user")]
	pub user: String,
	#[serde(default)]
	pub password: String,
}

impl Default for ClickHouseConfig {
	fn default() -> Self {
		Self {
			url: __default_clickhouse_url(),
			database: __default_clickhouse_database(),
			user: __default_clickhouse_user(),
			password: String::new(),
		}
	}
}
fn __default_clickhouse_url() -> String {
	"http://localhost:8123".to_string()
}
fn __default_clickhouse_database() -> String {
	"social_networks".to_string()
}
fn __default_clickhouse_user() -> String {
	"default".to_string()
}
