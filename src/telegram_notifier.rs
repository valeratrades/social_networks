use color_eyre::eyre::Result;
use reqwest::Client;
use serde_json::json;

use crate::config::TelegramConfig;

pub struct TelegramNotifier {
	config: TelegramConfig,
	client: Client,
}

impl TelegramNotifier {
	pub fn new(config: TelegramConfig) -> Self {
		Self { config, client: Client::new() }
	}

	pub async fn send_ping_notification(&self, username: &str, platform: &str) -> Result<()> {
		let text = format!("/Ping from: @{}, {}", username, platform);
		self.send_message(&text).await
	}

	async fn send_message(&self, text: &str) -> Result<()> {
		let url = format!("https://api.telegram.org/bot{}/sendMessage", self.config.bot_token);

		let payload = json!({
			"chat_id": self.config.chat_id,
			"text": text,
		});

		let response = self.client.post(&url).json(&payload).send().await?;

		if !response.status().is_success() {
			let error_text = response.text().await?;
			return Err(color_eyre::eyre::eyre!("Failed to send Telegram message: {}", error_text));
		}

		Ok(())
	}
}
