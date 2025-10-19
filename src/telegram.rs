use chrono::{Local, Timelike};
use clap::Args;
use color_eyre::eyre::Result;
use grammers_client::{Client, Config, SignInError, Update};
use grammers_session::Session;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

#[derive(Args)]
pub struct TelegramArgs {}

#[derive(Debug, Serialize, Deserialize)]
struct StatusDrop {
	status: String,
}

impl Default for StatusDrop {
	fn default() -> Self {
		Self { status: String::new() }
	}
}

pub fn main(config: AppConfig, _args: TelegramArgs) -> Result<()> {
	// Set up tracing with file logging (truncate old logs)
	let log_file = v_utils::xdg_state_file!("telegram.log");
	if log_file.exists() {
		std::fs::remove_file(&log_file)?;
	}
	let file_appender = tracing_appender::rolling::never(log_file.parent().unwrap(), log_file.file_name().unwrap());
	let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

	tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_max_level(tracing::Level::DEBUG).init();

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_telegram_monitor(&config).await {
				error!("Telegram monitor error: {e}");
				error!("Reconnecting in 10 minutes...");
				tokio::time::sleep(tokio::time::Duration::from_secs(10 * 60)).await;
			}
		}
	})
}

async fn run_telegram_monitor(config: &AppConfig) -> Result<()> {
	// Load or create session file (using username like Python code does)
	let session_filename = format!("{}.session", config.telegram.username);
	let session_file = v_utils::xdg_state_file!(&session_filename);
	info!("Using session file: {}", session_file.display());

	let session = if session_file.exists() {
		info!("Loading existing session");
		Session::load_file(&session_file)?
	} else {
		info!("Creating new session");
		Session::new()
	};

	// Load status drop
	let status_file = v_utils::xdg_state_file!("telegram_status.json");
	let status_drop: StatusDrop = if status_file.exists() {
		let content = std::fs::read_to_string(&status_file)?;
		let status: StatusDrop = serde_json::from_str(&content)?;
		info!("Loaded status from file: {}", status.status);
		status
	} else {
		info!("No status file found, using empty status");
		StatusDrop::default()
	};

	// Create client
	info!("Connecting to Telegram with api_id: {}", config.telegram.api_id);
	let client = Client::connect(Config {
		session,
		api_id: config.telegram.api_id,
		api_hash: config.telegram.api_hash.clone(),
		params: Default::default(),
	})
	.await?;
	info!("Connected to Telegram");

	// Sign in if not already
	if !client.is_authorized().await? {
		info!("Not authorized, requesting login code for {}", config.telegram.phone);
		let token = client.request_login_code(&config.telegram.phone).await?;
		info!("Login code requested successfully, check your Telegram app");
		eprintln!("Enter the code you received: ");
		let mut code = String::new();
		std::io::stdin().read_line(&mut code)?;
		let code = code.trim();

		info!("Attempting to sign in with code");
		match client.sign_in(&token, code).await {
			Ok(_) => {
				info!("Sign in successful");
			}
			Err(SignInError::PasswordRequired(password_token)) => {
				info!("2FA password required");
				eprintln!("Enter your 2FA password: ");
				let mut password = String::new();
				std::io::stdin().read_line(&mut password)?;
				let password = password.trim();
				client.check_password(password_token, password).await?;
				info!("2FA authentication successful");
			}
			Err(e) => {
				error!("Sign in error: {}", e);
				return Err(e.into());
			}
		}

		// Save session
		if let Some(parent) = session_file.parent() {
			std::fs::create_dir_all(parent)?;
		}
		client.session().save_to_file(&session_file)?;
		info!("Session saved to {}", session_file.display());
	}

	info!("--Telegram-- connected and authorized");

	// Resolve channel peer IDs
	info!("Resolving {} poll channels", config.telegram.poll_channels.len());
	let mut poll_peer_ids = Vec::new();
	for channel in &config.telegram.poll_channels {
		match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
			Some(chat) => {
				poll_peer_ids.push(chat.id());
				info!("Resolved poll channel: {} -> {}", channel, chat.id());
			}
			None => {
				error!("Could not resolve poll channel: {}", channel);
			}
		}
	}

	info!("Resolving {} info channels", config.telegram.info_channels.len());
	let mut info_peer_ids = Vec::new();
	for channel in &config.telegram.info_channels {
		match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
			Some(chat) => {
				info_peer_ids.push(chat.id());
				info!("Resolved info channel: {} -> {}", channel, chat.id());
			}
			None => {
				error!("Could not resolve info channel: {}", channel);
			}
		}
	}

	// Resolve watch channel
	info!("Resolving watch channel: {}", config.telegram.watch_channel_username);
	let watch_chat = match client.resolve_username(&config.telegram.watch_channel_username.trim_start_matches('@')).await? {
		Some(chat) => {
			info!("Watch channel resolved: {}", chat.id());
			chat.pack()
		}
		None => {
			error!("Could not resolve watch channel: {}", config.telegram.watch_channel_username);
			return Err(color_eyre::eyre::eyre!("Could not resolve watch channel: {}", config.telegram.watch_channel_username));
		}
	};

	let telegram_notifier = TelegramNotifier::new(config.telegram.clone());
	let mut last_heartbeat = 0i64;
	let mut last_status_update = 0i64;
	let mut message_counter = 0u64;

	info!("Starting main event loop");

	// Main event loop
	loop {
		let update = match client.next_update().await {
			Ok(u) => u,
			Err(e) => {
				error!("Error getting next update: {}", e);
				continue;
			}
		};
		debug!("Received update: {:?}", update);
		message_counter += 1;

		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				let chat = message.chat();
				let chat_id = chat.id();

				// Check if it's a DM with /ping
				let text = message.text();
				if text.contains("/ping") {
					// Check if it's from a user (not a channel/group)
					match chat {
						grammers_client::types::Chat::User(_) =>
							if let Some(sender) = message.sender() {
								let username = sender.username().unwrap_or("unknown");
								if let Err(e) = telegram_notifier.send_ping_notification(username, "Telegram").await {
									error!("Error sending notification: {}", e);
								} else {
									info!("Successfully sent notification for user: {}", username);
								}
							},
						_ => {}
					}
				}

				// Check if it's from a monitored channel
				if poll_peer_ids.contains(&chat_id) {
					if let Err(e) = handle_poll_message(&client, &message, watch_chat).await {
						error!("Error handling poll message: {}", e);
					}
				} else if info_peer_ids.contains(&chat_id) {
					if let Err(e) = handle_info_message(&client, &message, watch_chat).await {
						error!("Error handling info message: {}", e);
					}
				}
			}
			_ => {}
		}

		// Status update every 5 minutes
		let now = Local::now();
		if now.minute() % 5 == 0 {
			let current_time = now.timestamp();
			if current_time - last_status_update > 4 * 60 {
				if !status_drop.status.is_empty() {
					if let Err(e) = update_profile(&client, &status_drop.status).await {
						error!("Error updating profile: {}", e);
					} else {
						debug!("Profile status updated");
					}
				}
				last_status_update = current_time;
			}
		}

		// Heartbeat every hour
		if now.minute() % 60 == 0 {
			let current_time = now.timestamp();
			if current_time - last_heartbeat > 4 * 60 {
				let formatted_time = now.format("%m/%d/%y-%H");
				info!("Heartbeat received. Time: {}. Since last heartbeat processed: {} messages", formatted_time, message_counter);
				message_counter = 0;
				last_heartbeat = current_time;
			}
		}
	}
}

async fn handle_poll_message(client: &Client, message: &grammers_client::types::Message, watch_chat: grammers_client::types::PackedChat) -> Result<()> {
	// Check if message contains a poll or media
	if message.media().is_some() {
		// Forward poll messages to watch channel
		let source = message.chat().pack();
		client.forward_messages(watch_chat, &[message.id()], source).await?;
		info!("Forwarded poll/media message from {}", message.chat().name());
	}
	Ok(())
}

async fn handle_info_message(client: &Client, message: &grammers_client::types::Message, watch_chat: grammers_client::types::PackedChat) -> Result<()> {
	let key_words = [
		"самые торгуемые акции",
		"отслеживание настроений",
		"гугл тренд",
		"google trends",
		"поисковых запросов",
		"популярные запросы",
		"популярных запросов",
		"кредитное плечо",
		"закредитованность",
		"количество уникальных слов",
		"открытому интересу",
		"открытый интерес",
	];

	let text = message.text();
	let text_lower = text.to_lowercase();
	if key_words.iter().any(|word| text_lower.contains(word)) {
		// Forward message to watch channel
		let source = message.chat().pack();
		client.forward_messages(watch_chat, &[message.id()], source).await?;
		info!("Forwarded info message from {}", message.chat().name());
	}
	Ok(())
}

async fn update_profile(client: &Client, status: &str) -> Result<()> {
	use grammers_tl_types::functions;

	client
		.invoke(&functions::account::UpdateProfile {
			first_name: None,
			last_name: None,
			about: Some(status.to_string()),
		})
		.await?;

	Ok(())
}
