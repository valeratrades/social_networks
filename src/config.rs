use color_eyre::eyre::Result;
use tg::chat::TelegramDestination;
use v_utils::{io::ExpandedPath, macros::MyConfigPrimitives, trades::Timeframe};

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AppConfig {
	pub discord: DiscordConfig,
	pub telegram: TelegramConfig,
	pub twitter: TwitterConfig,
	pub youtube: YoutubeConfig,
	pub email: Option<EmailConfig>,
	#[serde(default)]
	pub clickhouse: ClickHouseConfig,
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct DiscordConfig {
	pub user_token: String,
	pub monitored_users: Vec<String>,
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
	pub poll_channels: Vec<String>,
	pub info_channels: Vec<String>,
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct TwitterConfig {
	pub bearer_token: String,
	pub everytime_polls_list: String,
	pub sometimes_polls_list: String,
	pub oauth: Option<TwitterOauthConfig>,
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
	pub channels: std::collections::HashMap<String, String>,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct EmailConfig {
	/// Gmail email address to monitor
	pub email: String,
	/// Google OAuth2 Client ID (from Google Cloud Console)
	pub client_id: String,
	/// Google OAuth2 Client Secret (from Google Cloud Console)
	pub client_secret: String,
	/// Path to store auth tokens (default: ~/.local/state/social_networks/gmail_tokens.json)
	#[serde(default = "__default_email_token_path")]
	#[primitives(skip)]
	pub token_path: String,
	/// Regex patterns to match against sender email to ignore (skip processing entirely)
	#[serde(default)]
	pub ignore_patterns: Vec<String>,
}

fn __default_email_token_path() -> String {
	let app_name = env!("CARGO_PKG_NAME");
	let xdg_dirs = xdg::BaseDirectories::with_prefix(app_name);
	xdg_dirs.place_state_file("gmail_tokens.json").unwrap().display().to_string()
}

#[derive(Clone, Debug, serde::Deserialize)]
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

impl AppConfig {
	pub fn read(path: Option<ExpandedPath>) -> Result<Self, config::ConfigError> {
		let mut builder = config::Config::builder().add_source(config::Environment::default());
		let settings: Self = match path {
			Some(path) => {
				let builder = builder.add_source(config::File::with_name(&path.to_string()).required(true));
				builder.build()?.try_deserialize()?
			}
			None => {
				let app_name = env!("CARGO_PKG_NAME");
				let xdg_dirs = xdg::BaseDirectories::with_prefix(app_name);
				let xdg_conf_dir = xdg_dirs.get_config_home().unwrap().parent().unwrap().display().to_string();

				let locations = [format!("{xdg_conf_dir}/{app_name}"), format!("{xdg_conf_dir}/{app_name}/config")];
				for location in locations.iter() {
					builder = builder.add_source(config::File::with_name(location).required(false));
				}
				let raw: config::Config = builder.build()?;

				match raw.try_deserialize() {
					Ok(settings) => settings,
					Err(e) => {
						eprintln!("Config file does not exist or is invalid:");
						return Err(e);
					}
				}
			}
		};

		Ok(settings)
	}
}
