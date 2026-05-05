use std::{collections::HashMap, convert::Infallible};

use clap::Args;
use color_eyre::eyre::Result;
use futures::future::{Either, select};
use grammers_client::update::Update;
use jiff::Timestamp;
use social_networks_utils::{
	config::AppConfig,
	telegram_notifier::TelegramNotifier,
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
};
use tokio::time::{self, Duration};
use tracing::{error, info};

use crate::client::{AdapterError, Client as AdapterClient};

const SURFACE: &str = "telegram_dms";
#[derive(Args)]
pub struct DmsArgs {}

pub struct TelegramDms {
	config: AppConfig,
	telegram_notifier: TelegramNotifier,
	last_message_times: HashMap<i64, Timestamp>,
	monitored_users: Vec<String>,
}

impl TelegramDms {
	pub fn new(config: AppConfig) -> Self {
		let telegram_notifier = TelegramNotifier::new(config.telegram.clone());
		let monitored_users = config.dms.monitored_users_for_telegram();

		Self {
			config,
			telegram_notifier,
			last_message_times: HashMap::new(),
			monitored_users,
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

	/// Run a single connect+listen cycle. Returns `Ok(())` on a recoverable disconnect,
	/// `Err(AdapterError::Auth)` on auth-class failures.
	async fn run_session(&mut self) -> Result<(), AdapterError> {
		let TelegramConnection { client, updates, mut runner } = match self.connect().await {
			Ok(c) => c,
			Err(e) => {
				if let Some(detail) = classify_telegram_auth_error(&e) {
					return Err(AdapterError::Auth { surface: SURFACE, detail });
				}
				error!("Telegram connection error: {e:#}");
				return Ok(());
			}
		};

		info!("--Telegram DM Commands-- connected and authorized");
		println!("Telegram DM Commands: Connected");

		let _ = client; // hold for the lifetime of the session
		let mut updates = Box::new(updates);

		loop {
			if telegram_utils::should_reconnect_for_stack() {
				return Ok(());
			}
			telegram_utils::log_stack("dms telegram before select");

			enum Event {
				Update(Box<Result<Update, grammers_client::InvocationError>>),
				RunnerExited,
			}

			let event = {
				let update_fut = std::pin::pin!(updates.next());
				let runner_fut = runner.as_mut();
				match select(update_fut, runner_fut).await {
					Either::Left((result, _)) => Event::Update(Box::new(result)),
					Either::Right(((), _)) => Event::RunnerExited,
				}
			};

			telegram_utils::log_stack("dms telegram after select");

			match event {
				Event::RunnerExited => {
					error!("MTProto runner exited unexpectedly, reconnecting...");
					return Ok(());
				}
				Event::Update(result) => match *result {
					Err(e) => {
						let s = format!("{e:#}");
						if classify_invocation_auth(&s) {
							return Err(AdapterError::Auth { surface: SURFACE, detail: s });
						}
						error!("Error getting next update: {s}, reconnecting...");
						return Ok(());
					}
					Ok(update) => self.handle_update(update).await,
				},
			}
		}
	}

	async fn handle_update(&mut self, update: Update) {
		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				let peer = match message.peer() {
					Some(p) => p,
					None => {
						error!("Skipping message with unresolved peer: ");
						return;
					}
				};

				if !matches!(peer, grammers_client::peer::Peer::User(_)) {
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
}

impl AdapterClient for TelegramDms {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		loop {
			self.run_session().await?;
			error!("Telegram DMs reconnecting in 30s...");
			time::sleep(Duration::from_secs(30)).await;
		}
	}
}

/// Inspect a `color_eyre` connect-time error string for auth-class failures.
/// `SignInError` non-`PasswordRequired` variants and known unauthorized RPC
/// errors all surface as text containing one of these tokens.
pub(crate) fn classify_telegram_auth_error(e: &color_eyre::eyre::Report) -> Option<String> {
	let s = format!("{e:#}");
	if classify_invocation_auth(&s) || s.to_lowercase().contains("sign in failed") {
		Some(s)
	} else {
		None
	}
}

/// Match the canonical RPC error names that grammers stringifies for codes 401/403/303.
/// String-matching is required because grammers 0.9 doesn't expose typed variants.
pub(crate) fn classify_invocation_auth(s: &str) -> bool {
	let lc = s.to_lowercase();
	lc.contains("auth_key_unregistered")
		|| lc.contains("session_revoked")
		|| lc.contains("session_expired")
		|| lc.contains("user_deactivated")
		|| lc.contains("auth_key_invalid")
		|| lc.contains("user_deactivated_ban")
		|| lc.contains("api_id_invalid")
		|| lc.contains("phone_number_banned")
}
