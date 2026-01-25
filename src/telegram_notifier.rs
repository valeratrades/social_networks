use color_eyre::eyre::{Result, bail};
use reqwest::Client;
use tracing::instrument;

use crate::config::{TelegramConfig, TelegramDestination};

#[derive(Clone, Debug)]
pub struct TelegramNotifier {
	config: TelegramConfig,
	client: Client,
}

impl TelegramNotifier {
	pub fn new(config: TelegramConfig) -> Self {
		Self { config, client: Client::new() }
	}

	pub async fn send_ping_notification(&self, username: &str, platform: &str) -> Result<()> {
		let text = format!("/Ping from: @{username}, {platform}");
		self.send_message_to_alerts(&text).await
	}

	pub async fn send_monitored_user_message(&self, username: &str, platform: &str) -> Result<()> {
		let text = format!("New message from @{username}, {platform}");
		self.send_message_to_alerts(&text).await
	}

	pub async fn send_twitter_poll(&self, author: &str, text: &str, tweet_id: &str) -> Result<()> {
		let message = format!("Twitter poll from {author}:\n{text}\n\nhttps://twitter.com/twitter/statuses/{tweet_id}");
		self.send_message_to_output(&message).await
	}

	pub async fn send_youtube_notification(&self, channel_name: &str, title: &str, sentiment: &str, video_id: &str) -> Result<()> {
		let message = format!("[{channel_name}] uploaded a new video: [{title}]\nPerception: {sentiment}\n\nhttps://youtube.com/watch?v={video_id}");
		self.send_message_to_output(&message).await
	}

	#[instrument(skip_all)]
	pub async fn send_message_to_alerts(&self, text: &str) -> Result<()> {
		self.send_message(text, &self.config.channel_alerts).await
	}

	#[instrument(skip_all)]
	async fn send_message_to_output(&self, text: &str) -> Result<()> {
		self.send_message(text, &self.config.channel_output).await
	}

	#[instrument(skip_all)]
	async fn send_message(&self, text: &str, destination: &TelegramDestination) -> Result<()> {
		let url = format!("https://api.telegram.org/bot{}/sendMessage", self.config.bot_token);

		let mut params = vec![("text", text.to_string())];
		params.extend(destination.destination_params());
		tracing::debug!(?params);

		let response = self.client.post(&url).form(&params).send().await?;
		tracing::debug!(?response);

		if !response.status().is_success() {
			let error_text = response.text().await?;
			bail!("Failed to send Telegram message: {error_text}");
		}

		Ok(())
	}
}
