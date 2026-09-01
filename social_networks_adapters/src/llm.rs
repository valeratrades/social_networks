use color_eyre::eyre::{Result, bail};
use v_utils::macros::MyConfigPrimitives;

/// Keys for the providers `ask_llm` can reach. Which one a call needs follows from the
/// [`ask_llm::Model`] tier it asks for, so a key absent here surfaces as `ask_llm::MissingToken`
/// on the request that wanted it.
#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct LlmConfig {
	#[serde(default)]
	pub claude_token: Option<String>,
	#[serde(default)]
	pub deepseek_token: Option<String>,
	#[serde(default)]
	pub openai_token: Option<String>,
}

impl LlmConfig {
	pub fn assert_any_key(&self) -> Result<()> {
		if self.claude_token.is_none() && self.deepseek_token.is_none() && self.openai_token.is_none() {
			bail!("`[llm]` carries no key — give it at least one of claude_token, deepseek_token, openai_token, or drop the section");
		}
		Ok(())
	}
}

impl From<&LlmConfig> for ask_llm::config::AppConfig {
	fn from(config: &LlmConfig) -> Self {
		Self {
			claude_token: config.claude_token.clone(),
			deepseek_token: config.deepseek_token.clone(),
			openai_token: config.openai_token.clone(),
		}
	}
}
