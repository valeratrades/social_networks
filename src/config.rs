use color_eyre::eyre::Result;
use tg::chat::TelegramDestination;
use v_utils::{io::ExpandedPath, macros::MyConfigPrimitives};

#[derive(Clone, Debug, Default, serde::Deserialize)]
pub struct AppConfig {
	pub discord: DiscordConfig,
	pub telegram: TelegramConfig,
	pub twitter: TwitterConfig,
	pub youtube: YoutubeConfig,
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
}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct YoutubeConfig {
	pub channels: std::collections::HashMap<String, String>,
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
