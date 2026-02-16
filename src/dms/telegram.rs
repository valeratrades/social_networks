use std::collections::HashMap;

use color_eyre::eyre::Result;
use futures::future::{Either, select};
use grammers_client::{Client, Update};
use jiff::Timestamp;
use tokio::time::{self, Duration};
use tracing::{error, info};

use crate::{
	config::AppConfig,
	telegram_notifier::TelegramNotifier,
	telegram_utils::{self, ConnectionConfig, RunnerFuture, TelegramConnection},
};

type UpdateStream = grammers_client::client::updates::UpdateStream;
pub struct TelegramMonitor {
	config: AppConfig,
	state: State,
	client: Option<Client>,
	telegram_notifier: TelegramNotifier,
	last_message_times: HashMap<i64, Timestamp>,
	monitored_users: Vec<String>,
}
impl TelegramMonitor {
	pub fn new(config: AppConfig) -> Self {
		let telegram_notifier = TelegramNotifier::new(config.telegram.clone());
		let monitored_users = config.dms.monitored_users_for_telegram();

		Self {
			config,
			state: State::Disconnected,
			client: None,
			telegram_notifier,
			last_message_times: HashMap::new(),
			monitored_users,
		}
	}

	pub async fn collect(&mut self) -> Result<()> {
		match &mut self.state {
			State::Disconnected => {
				match self.connect().await {
					Ok(TelegramConnection { client, updates, runner }) => {
						info!("--Telegram DM Commands-- connected and authorized");
						println!("Telegram DM Commands: Connected");
						self.client = Some(client);
						self.state = State::Connected { updates: Box::new(updates), runner };
					}
					Err(e) => {
						error!("Telegram connection error: {e}");
						error!("Retrying in 10 minutes...");
						time::sleep(Duration::from_secs(10 * 60)).await;
					}
				}
				Ok(())
			}
			State::Connected { updates, runner } => {
				// Check stack usage and force reconnect if critical
				if telegram_utils::should_reconnect_for_stack() {
					self.state = State::Disconnected;
					self.client = None;
					return Ok(());
				}

				telegram_utils::log_stack("dms telegram before select");

				enum Event {
					Update(Result<Update, grammers_client::InvocationError>),
					RunnerExited,
				}

				let event = {
					let update_fut = std::pin::pin!(updates.next());
					let runner_fut = runner.as_mut();

					match select(update_fut, runner_fut).await {
						Either::Left((result, _)) => Event::Update(result),
						Either::Right(((), _)) => Event::RunnerExited,
					}
				};

				telegram_utils::log_stack("dms telegram after select");

				match event {
					Event::RunnerExited => {
						error!("MTProto runner exited unexpectedly, reconnecting...");
						self.state = State::Disconnected;
						self.client = None;
						return Ok(());
					}
					Event::Update(Err(e)) => {
						error!("Error getting next update: {e}, reconnecting...");
						self.state = State::Disconnected;
						self.client = None;
						return Ok(());
					}
					Event::Update(Ok(update)) => {
						self.handle_update(update).await;
					}
				}
				Ok(())
			}
		}
	}

	async fn handle_update(&mut self, update: Update) {
		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				let peer = match message.peer() {
					Ok(p) => p,
					Err(e) => {
						error!("Skipping message with unresolved peer: {e:?}");
						return;
					}
				};

				// Only process DMs (user peers)
				if !matches!(peer, grammers_client::types::Peer::User(_)) {
					return;
				}

				let text = message.text();
				let sender = match message.sender() {
					Some(s) => s,
					None => return,
				};
				let username = sender.username().unwrap_or("unknown");
				let chat_id = peer.id().bot_api_dialog_id();
				let now = Timestamp::now();

				let has_ping = text.contains("/ping");
				let is_monitored_user = self.monitored_users.contains(&username.to_string());

				if has_ping {
					if let Err(e) = self.telegram_notifier.send_ping_notification(username, "Telegram").await {
						error!("Error sending ping notification: {e}");
					} else {
						info!("Successfully sent ping notification for user: {username}");
					}
				} else if is_monitored_user {
					let last_message_time = self.last_message_times.get(&chat_id).copied();

					let should_notify = if let Some(last_time) = last_message_time {
						let duration_since_last = now.duration_since(last_time);
						duration_since_last.as_secs() >= 15 * 60
					} else {
						true
					};

					if should_notify {
						println!("Telegram message from monitored user {username}");
						if let Err(e) = self.telegram_notifier.send_monitored_user_message(username, "Telegram").await {
							error!("Error sending monitored user notification: {e}");
						} else {
							info!("Successfully sent monitored user notification for: {username}");
						}
					}

					self.last_message_times.insert(chat_id, now);
				}
			}
			_ => {}
		}
	}

	async fn connect(&self) -> Result<TelegramConnection> {
		telegram_utils::connect(ConnectionConfig {
			username: &self.config.telegram.username,
			phone: &self.config.telegram.phone,
			api_id: self.config.telegram.api_id,
			api_hash: &self.config.telegram.api_hash,
			session_suffix: "_dm",
		})
		.await
	}
}

enum State {
	Disconnected,
	Connected { updates: Box<UpdateStream>, runner: RunnerFuture },
}
