use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr, eyre};
use social_networks_utils::db::Database;
use tg::TelegramDestination;
use tracing::{instrument, warn};

use crate::{
	notify::{
		Level, Message, Rich,
		telegram::{BASE, Bot, Target},
	},
	telegram_dms::TelegramConfig,
};

#[derive(Clone, Debug)]
pub struct TelegramNotifier {
	config: TelegramConfig,
	bot: Bot,
}

impl TelegramNotifier {
	pub fn new(config: TelegramConfig) -> Self {
		let bot = Bot::try_new(BASE, config.bot_token.clone().into()).expect("a TLS client builds wherever the OS has its certificates");
		Self { config, bot }
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
		let mut message = Rich::from(format!("Twitter poll from {author}:\n{text}\n\n"));
		message.url(&format!("https://twitter.com/twitter/statuses/{tweet_id}"));
		self.send_message_to_output(message).await
	}

	pub async fn send_youtube_notification(&self, channel_name: &str, title: &str, sentiment: &str, video_id: &str) -> Result<()> {
		let mut message = Rich::from(format!("[{channel_name}] uploaded a new video: [{title}]\nPerception: {sentiment}\n\n"));
		message.url(&format!("https://youtube.com/watch?v={video_id}"));
		self.send_message_to_output(message).await
	}

	/// A module reporting "still alive, but something is wrong": a warning, never an error. A failed
	/// delivery is logged and swallowed: a broken alerting path must not kill surfaces that still work.
	#[instrument(skip_all)]
	pub async fn report_recoverable(&self, surface: &str, detail: &str) {
		let text = format!("[social_networks] {surface}: {detail}");
		warn!("{text}");
		let delivered = match self.owner_chat_id().await {
			Ok(chat_id) => self.send(Target { chat_id, topic_id: None }, text.into()).await,
			Err(e) => Err(e),
		};
		if let Err(e) = delivered {
			warn!("failed to deliver recoverable report: {e:#}");
		}
	}

	/// The Bot API refuses to resolve a *user* `@name` (`getChat` answers "chat not found"), and
	/// a user's private chat id is their user id, so we lift it off any update they authored, once:
	/// updates are only retained ~24h, so the id is kept in the db past them.
	async fn owner_chat_id(&self) -> Result<i64> {
		let owner = self.config.username.trim_start_matches('@');
		let key = format!("telegram_owner_chat_id:{owner}");
		let db = Database::try_new().await?;
		if let Some(chat_id) = db.state(&key).await? {
			return Ok(chat_id);
		}

		let updates = self.bot.updates(0, Duration::ZERO).await?;
		let chat_id = updates
			.iter()
			.filter_map(|(_, u)| u.get("message")?.get("from"))
			.find(|from| from.get("username").and_then(|u| u.as_str()) == Some(owner))
			.and_then(|from| from.get("id").and_then(|id| id.as_i64()))
			.ok_or_else(|| eyre!("no update from @{owner} in the bot's retention window; message the bot once so it learns the chat id"))?;
		db.set_state(&key, &chat_id).await?;
		Ok(chat_id)
	}

	#[instrument(skip_all)]
	pub async fn send_message_to_alerts(&self, text: &str) -> Result<()> {
		self.send(self.target(&self.config.channel_alerts).await?, text.into()).await
	}

	#[instrument(skip_all)]
	async fn send_message_to_output(&self, text: Rich) -> Result<()> {
		self.send(self.target(&self.config.channel_output).await?, text).await
	}

	async fn target(&self, destination: &TelegramDestination) -> Result<Target> {
		let (TelegramDestination::Channel(chat) | TelegramDestination::Group(chat) | TelegramDestination::GroupTopic { group: chat, .. }) = destination;
		let chat = chat.chat_id_param();
		let chat_id = match chat.starts_with('@') {
			true => self.bot.chat(&chat).await?.id,
			false => chat.parse().wrap_err_with(|| format!("`{chat}` is no chat id"))?,
		};
		let topic_id = destination.topic_id().map(i64::try_from).transpose()?;
		Ok(Target { chat_id, topic_id })
	}

	#[instrument(skip_all)]
	async fn send(&self, to: Target, text: Rich) -> Result<()> {
		let message = Message {
			title: None,
			text,
			level: Level::Normal,
			links: Vec::new(),
			buttons: Vec::new(),
			photo: None,
			unsubscribe: None,
		};
		self.bot.send(to, &message).await.wrap_err("Failed to send Telegram message")?;
		Ok(())
	}
}
