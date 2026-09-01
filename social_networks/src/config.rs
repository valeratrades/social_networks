use social_networks_adapters::{email::EmailConfig, llm::LlmConfig, telegram_dms::TelegramConfig, twitter::TwitterConfig, youtube::YoutubeConfig};
use social_networks_utils::skool::SkoolCredentials;
use v_utils::macros::{LiveSettings, MyConfigPrimitives, Settings};

use crate::{dms::DmsConfig, rolodex::RolodexConfig};

#[derive(Clone, Debug, Default, LiveSettings, MyConfigPrimitives, Settings)]
pub struct AppConfig {
	/// Required by the surfaces that reason: youtube, email and rolodex
	#[settings(skip)]
	#[serde(default)]
	pub llm: Option<LlmConfig>,
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
	/// Only `rolodex dm --skool` needs it — reads are what is public either way
	#[settings(skip)]
	#[serde(default)]
	pub skool: Option<SkoolConfig>,
	#[settings(skip)]
	#[serde(default)]
	pub rolodex: Option<RolodexConfig>,
}
impl AppConfig {
	pub fn require_llm(&self, surface: &'static str) -> color_eyre::Result<LlmConfig> {
		self.llm
			.clone()
			.ok_or_else(|| color_eyre::eyre::eyre!("the {surface} surface reasons about what it sees, so it needs an `[llm]` section in the config"))
	}
}

/// Lives here rather than beside [`SkoolCredentials`] because the env indirection is the binary's
/// config machinery, which `social_networks_utils` does not depend on.
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct SkoolConfig {
	pub email: String,
	pub password: String,
}
impl From<&SkoolConfig> for SkoolCredentials {
	fn from(config: &SkoolConfig) -> Self {
		Self {
			email: config.email.clone(),
			password: config.password.clone(),
		}
	}
}
