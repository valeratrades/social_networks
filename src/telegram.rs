use std::sync::Arc;

use clap::Args;
use color_eyre::eyre::Result;
use grammers_client::{Client, SignInError, Update, UpdatesConfiguration};
use grammers_mtsender::SenderPool;
use grammers_session::{defs::PeerRef, storages::SqliteSession};
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
	println!("Starting Telegram...");
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

	info!("Opening session database");
	let session = match SqliteSession::open(&session_file) {
		Ok(s) => Arc::new(s),
		Err(e) => {
			// Check if it's a database corruption error (SQLite error code 26: SQLITE_NOTADB)
			let err_str = e.to_string();
			if err_str.contains("not a database") || err_str.contains("code 26") {
				error!("Session database is corrupted: {e}");
				info!("Deleting corrupted session file and creating a new one");
				std::fs::remove_file(&session_file)?;
				Arc::new(SqliteSession::open(&session_file)?)
			} else {
				return Err(e.into());
			}
		}
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
	let pool = SenderPool::new(Arc::clone(&session), config.telegram.api_id);
	let client = Client::new(&pool);
	let SenderPool { runner, updates, .. } = pool;
	let _pool_task = tokio::spawn(runner.run());
	info!("Connected to Telegram");

	// Sign in if not already
	if !client.is_authorized().await? {
		info!("Not authorized, requesting login code for {}", config.telegram.phone);
		let token = client.request_login_code(&config.telegram.phone, &config.telegram.api_hash).await?;
		info!("Login code requested successfully, check your Telegram app");

		println!("Enter the code you received: ");
		let mut code = String::new();
		std::io::stdin().read_line(&mut code)?;
		let code = code.trim();
		println!("Code received, authenticating...");
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

		// SqliteSession saves automatically, no need to manually save
		eprintln!("Session saved successfully");
		info!("Session saved successfully to {}", session_file.display());
	}

	println!("Telegram started");
	info!("--Telegram-- connected and authorized");

	// Resolve channel peer IDs
	info!("Resolving {} poll channels", config.telegram.poll_channels.len());
	let mut poll_peer_ids = Vec::new();
	for channel in &config.telegram.poll_channels {
		match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
			Some(peer) => {
				poll_peer_ids.push(peer.id());
				info!("Resolved poll channel: {} -> {}", channel, peer.id().bot_api_dialog_id());
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
			Some(peer) => {
				info_peer_ids.push(peer.id());
				info!("Resolved info channel: {} -> {}", channel, peer.id().bot_api_dialog_id());
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
		Some(peer) => {
			info!("Output channel resolved: {}", peer.id().bot_api_dialog_id());
			PeerRef::from(peer)
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
	let mut updates = client.stream_updates(
		updates,
		UpdatesConfiguration {
			catch_up: true,
			..Default::default()
		},
	);
	loop {
		let update = match updates.next().await {
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
				let peer = message.peer().unwrap();
				let peer_id = peer.id();

				// Check if it's a DM with /ping
				let text = message.text();
				if text.contains("/ping") {
					// Check if it's from a user (not a channel/group)
					if let Some(sender) = message.sender()
						&& matches!(peer, grammers_client::types::Peer::User(_))
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
				if poll_peer_ids.contains(&peer_id) {
					if let Err(e) = handle_poll_message(&client, &message, watch_chat).await {
						error!("Error handling poll message: {e}");
					}
				} else if info_peer_ids.contains(&peer_id)
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

async fn handle_poll_message(client: &Client, message: &grammers_client::types::Message, watch_chat: PeerRef) -> Result<()> {
	// Check if message contains a poll or media
	if message.media().is_some() {
		// Forward poll messages to watch channel
		let source = message.peer().unwrap();
		client.forward_messages(watch_chat, &[message.id()], PeerRef::from(source)).await?;
		info!("Forwarded poll/media message from {}", source.name().unwrap_or("unknown"));
	}
	Ok(())
}

async fn handle_info_message(client: &Client, message: &grammers_client::types::Message, watch_chat: PeerRef) -> Result<()> {
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
		let source = message.peer().unwrap();
		client.forward_messages(watch_chat, &[message.id()], PeerRef::from(source)).await?;
		info!("Forwarded info message from {}", source.name().unwrap_or("unknown"));
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
