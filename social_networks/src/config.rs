use social_networks_adapters::{
	discord_mirror::MirrorConfig, email::EmailConfig, llm::LlmConfig, skool::SkoolCredentials, telegram_dms::TelegramConfig, twitter::TwitterConfig, youtube::YoutubeConfig,
};
use social_networks_reach::RolodexConfig;
use v_utils::macros::{LiveSettings, MyConfigPrimitives, Settings};

use crate::dms::DmsConfig;

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
	/// `dm --skool` signs in with it, and `recon` sees no group at all without it — reading a *person*
	/// is what is public either way
	#[settings(skip)]
	#[serde(default)]
	pub skool: Option<SkoolCredentials>,
	#[settings(skip)]
	#[serde(default)]
	pub rolodex: Option<RolodexConfig>,
	/// The token stays at `[dms.discord]`: it is one account.
	#[settings(skip)]
	#[serde(default)]
	pub mirror: Option<MirrorConfig>,
}
impl AppConfig {
	pub fn require_llm(&self, surface: &'static str) -> color_eyre::Result<LlmConfig> {
		self.llm
			.clone()
			.ok_or_else(|| color_eyre::eyre::eyre!("the {surface} surface reasons about what it sees, so it needs an `[llm]` section in the config"))
	}
}
