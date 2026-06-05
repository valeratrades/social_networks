use social_networks_adapters::{email::EmailConfig, telegram_dms::TelegramConfig, twitter::TwitterConfig, youtube::YoutubeConfig};
use v_utils::macros::{LiveSettings, MyConfigPrimitives, Settings};

use crate::dms::DmsConfig;

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
}
