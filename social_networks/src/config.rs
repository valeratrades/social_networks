use std::path::PathBuf;

use social_networks_adapters::{email::EmailConfig, llm::LlmConfig, skool::SkoolCredentials, telegram_dms::TelegramConfig, twitter::TwitterConfig, youtube::YoutubeConfig};
use social_networks_reach::purpose::Purposes;
use v_utils::macros::{LiveSettings, MyConfigPrimitives, Settings};

use crate::dms::DmsConfig;

#[derive(Clone, Debug, Default, LiveSettings, MyConfigPrimitives, Settings)]
#[primitives(skip_serialize)]
pub struct AppConfig {
	/// Required by the surfaces that reason: youtube, email and a purpose's `pull`
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
	pub email: Vec<EmailConfig>,
	/// `dm --skool` signs in with it, and `recon` sees no group at all without it — reading a *person*
	/// is what is public either way
	#[settings(skip)]
	#[serde(default)]
	pub skool: Option<SkoolCredentials>,
	/// Every venue transcript `recon` writes, shared by every purpose that procures from one
	#[settings(skip)]
	#[serde(default)]
	pub venues: Option<PathBuf>,
	/// Name → what the people in that folder are for. `rolodex` is the one the `rolodex` command means
	#[settings(skip)]
	#[serde(default)]
	pub purposes: Purposes,
}
impl AppConfig {
	pub fn require_llm(&self, surface: &'static str) -> color_eyre::Result<LlmConfig> {
		self.llm
			.clone()
			.ok_or_else(|| color_eyre::eyre::eyre!("the {surface} surface reasons about what it sees, so it needs an `[llm]` section in the config"))
	}
}
