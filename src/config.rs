use color_eyre::eyre::Result;
use tg::chat::TelegramDestination;
use v_utils::{io::ExpandedPath, macros::MyConfigPrimitives};

#[derive(Debug, Default, derive_new::new, Clone, serde::Deserialize)]
pub struct AppConfig {
	pub discord: DiscordConfig,
	pub telegram: TelegramConfig,
}

#[derive(Debug, Default, Clone, derive_new::new, MyConfigPrimitives)]
pub struct DiscordConfig {
	pub user_token: String,
	pub monitored_users: Vec<String>,
	pub my_username: String,
}

#[derive(Debug, Default, Clone, derive_new::new, MyConfigPrimitives)]
pub struct TelegramConfig {
	pub bot_token: String,
	#[private_value]
	pub alerts_channel: TelegramDestination,
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
