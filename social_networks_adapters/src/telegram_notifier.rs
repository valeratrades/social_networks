use color_eyre::eyre::{Result, bail, eyre};
use reqwest::Client;
use tracing::{error, instrument};

use crate::telegram_dms::TelegramConfig;

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

	pub async fn send_call_notification(&self, caller: &str, platform: &str) -> Result<()> {
		let text = format!("Call from: {caller}, {platform}");
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

	/// A module reporting "still alive, but something is wrong". Delivery failures are swallowed
	/// with an `error!`, same policy as [`crate::client::alert`]: a broken alerting path must
	/// not kill surfaces that still work.
	#[instrument(skip_all)]
	pub async fn report_recoverable(&self, surface: &str, detail: &str) {
		let text = format!("[social_networks] {surface}: {detail}");
		error!("{text}");
		let delivered = match self.owner_chat_id().await {
			Ok(chat_id) => self.send_message(&text, vec![("chat_id", chat_id.to_string())]).await,
			Err(e) => Err(e),
		};
		if let Err(e) = delivered {
			error!("failed to deliver recoverable report: {e:#}");
		}
	}

	/// The Bot API refuses to resolve a *user* `@name` (`getChat` answers "chat not found"), and
	/// a user's private chat id is their user id, so we lift it off any update they authored.
	/// Requires the owner to have messaged the bot within its ~24h update retention.
	async fn owner_chat_id(&self) -> Result<i64> {
		let url = format!("https://api.telegram.org/bot{}/getUpdates", self.config.bot_token);
		let response: serde_json::Value = self.client.get(&url).send().await?.json().await?;
		let updates = response.get("result").and_then(|r| r.as_array()).ok_or_else(|| eyre!("getUpdates returned {response}"))?;

		let owner = self.config.username.trim_start_matches('@');
		updates
			.iter()
			.filter_map(|u| u.get("message")?.get("from"))
			.find(|from| from.get("username").and_then(|u| u.as_str()) == Some(owner))
			.and_then(|from| from.get("id").and_then(|id| id.as_i64()))
			.ok_or_else(|| eyre!("no update from @{owner} in the bot's retention window; message the bot once so it learns the chat id"))
	}

	#[instrument(skip_all)]
	pub async fn send_message_to_alerts(&self, text: &str) -> Result<()> {
		self.send_message(text, self.config.channel_alerts.destination_params()).await
	}

	#[instrument(skip_all)]
	async fn send_message_to_output(&self, text: &str) -> Result<()> {
		self.send_message(text, self.config.channel_output.destination_params()).await
	}

	#[instrument(skip_all)]
	async fn send_message(&self, text: &str, chat_params: Vec<(&str, String)>) -> Result<()> {
		let url = format!("https://api.telegram.org/bot{}/sendMessage", self.config.bot_token);

		let mut params = vec![("text", text.to_string())];
		params.extend(chat_params);
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
