use v_utils::macros::MyConfigPrimitives;

/// Overrides for the providers `ask_llm` can reach, each optional: the `claude` CLI resolves its own
/// login without one. A key named by an unset env var is absent, so a missing one surfaces on the
/// request that wanted it rather than at load.
#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct LlmConfig {
	#[serde(default)]
	#[private_value]
	pub claude_token: Option<String>,
	#[serde(default)]
	#[private_value]
	pub openai_token: Option<String>,
}

impl From<&LlmConfig> for ask_llm::config::AppConfig {
	fn from(config: &LlmConfig) -> Self {
		Self {
			claude_token: config.claude_token.clone(),
			openai_token: config.openai_token.clone(),
		}
	}
}
