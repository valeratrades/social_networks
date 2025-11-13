use clap::Args;
use color_eyre::eyre::Result;
use grammers_client::{Client, Config, SignInError, Update};
use grammers_session::Session;
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

#[derive(Args)]
pub struct TelegramArgs {}

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatusDrop {
	status: String,
}

pub fn main(config: AppConfig, _args: TelegramArgs) -> Result<()> {
	v_utils::clientside!("telegram");

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_telegram_monitor(&config).await {
				error!("Telegram monitor error: {e}\nReconnecting in 10 minutes...");
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

		print!("Enter the code you received: ");
		std::io::Write::flush(&mut std::io::stderr())?;
		let mut code = String::new();
		std::io::stdin().read_line(&mut code)?;
		let code = code.trim();
		eprintln!("Code received, authenticating...");
		info!("Received code from user (length: {})", code.len());
		debug!("Code value: '{}'", code);

		match client.sign_in(&token, code).await {
			Ok(_) => {
				eprintln!("Sign in successful! Saving session...");
				info!("Sign in successful");
			}
			Err(SignInError::PasswordRequired(password_token)) => {
				info!("2FA password required");
				print!("Enter your 2FA password: ");
				std::io::Write::flush(&mut std::io::stderr())?;
				let mut password = String::new();
				std::io::stdin().read_line(&mut password)?;
				let password = password.trim();
				eprintln!("Password received, checking 2FA...");
				info!("Received 2FA password from user");
				debug!("Password length: {}", password.len());

				client.check_password(password_token, password).await?;
				eprintln!("2FA authentication successful! Saving session...");
				info!("2FA authentication successful");
			}
			Err(e) => {
				error!("Sign in failed with error: {e}");
				return Err(e.into());
			}
		}

		// Save session
		info!("Saving session to {}", session_file.display());
		let session_to_save = client.session();
		debug!("Session object retrieved from client");

		// Try to save as bytes directly to debug
		let session_data = session_to_save.save();
		info!("Session serialized to {} bytes", session_data.len());

		match std::fs::write(&session_file, &session_data) {
			Ok(_) => {
				eprintln!("Session saved successfully");
				info!("Session saved successfully to {}", session_file.display());
			}
			Err(e) => {
				eprintln!("Failed to save session: {}", e);
				error!("Failed to save session file: {} (error: {})", session_file.display(), e);
				error!("Session file parent exists: {}", session_file.parent().map(|p| p.exists()).unwrap_or(false));
				return Err(e.into());
			}
		}
	}

	eprintln!("Connected and authorized, starting event loop...");
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

	// Resolve output channel - extract username from TelegramDestination
	let output_username = match &config.telegram.channel_output {
		tg::chat::TelegramDestination::ChannelUsername(username) => username.trim_start_matches('@'),
		tg::chat::TelegramDestination::ChannelExactUid(_) | tg::chat::TelegramDestination::Group { .. } => {
			return Err(color_eyre::eyre::eyre!("channel_output must be a username for grammers client forwarding"));
		}
	};

	info!("Resolving output channel: {output_username}");
	let watch_chat = match client.resolve_username(output_username).await? {
		Some(chat) => {
			info!("Output channel resolved: {}", chat.id());
			chat.pack()
		}
		None => {
			error!("Could not resolve output channel: {output_username}");
			return Err(color_eyre::eyre::eyre!("Could not resolve output channel: {output_username}"));
		}
	};

	eprintln!("Listening for messages...");
	info!("Starting main event loop");

	// Main event loop
	let telegram_notifier = TelegramNotifier::new(config.telegram.clone());
	let mut message_counter = 0u64;
	let mut last_status_update = Timestamp::default();
	loop {
		let update = match client.next_update().await {
			Ok(u) => u,
			Err(e) => {
				error!("Error getting next update: {e}");
				continue;
			}
		};
		debug!("Received update: {update:?}");
		message_counter += 1;

		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				let chat = message.chat();
				let chat_id = chat.id();

				// Check if it's a DM with /ping
				let text = message.text();
				if text.contains("/ping") {
					// Check if it's from a user (not a channel/group)
					if let grammers_client::types::Chat::User(_) = chat
						&& let Some(sender) = message.sender()
					{
						let username = sender.username().unwrap_or("unknown");
						if let Err(e) = telegram_notifier.send_ping_notification(username, "Telegram").await {
							error!("Error sending notification: {e}");
						} else {
							info!("Successfully sent notification for user: {username}");
						}
					}
				}

				// Check if it's from a monitored channel
				if poll_peer_ids.contains(&chat_id) {
					if let Err(e) = handle_poll_message(&client, &message, watch_chat).await {
						error!("Error handling poll message: {e}");
					}
				} else if info_peer_ids.contains(&chat_id)
					&& let Err(e) = handle_info_message(&client, &message, watch_chat).await
				{
					error!("Error handling info message: {e}");
				}
			}
			_ => {}
		}

		// Status update every 5 minutes

		let now = Timestamp::now();
		if now.duration_since(last_status_update) > SignedDuration::from_secs(4 * 60) {
			if !status_drop.status.is_empty() {
				if let Err(e) = update_profile(&client, &status_drop.status).await {
					error!("Error updating profile: {e}");
				} else {
					debug!("Profile status updated; message counter: {message_counter}");
				}
			}
			last_status_update = now;
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
