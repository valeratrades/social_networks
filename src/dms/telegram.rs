use std::{collections::HashMap, sync::Arc};

use color_eyre::eyre::Result;
use grammers_client::{Client, SignInError, Update, UpdatesConfiguration};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use jiff::Timestamp;
use tokio::time::{self, Duration};
use tracing::{debug, error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

type UpdateStream = grammers_client::client::updates::UpdateStream;

enum State {
	Disconnected,
	Connected { updates: Box<UpdateStream> },
}

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
					Ok((client, updates)) => {
						info!("--Telegram DM Commands-- connected and authorized");
						println!("Telegram DM Commands: Connected");
						self.client = Some(client);
						self.state = State::Connected { updates: Box::new(updates) };
					}
					Err(e) => {
						error!("Telegram connection error: {e}");
						error!("Retrying in 10 minutes...");
						time::sleep(Duration::from_secs(10 * 60)).await;
					}
				}
				Ok(())
			}
			State::Connected { updates } => {
				// Preemptive stack check: if we're using more than 6MB of stack (out of 8MB),
				// force a reconnect to reset the stack before we overflow.
				// Stack overflows are fatal and can't be caught by catch_unwind.
				let (stack_used, _) = crate::utils::stack_usage();
				if stack_used > 6 * 1024 * 1024 {
					crate::utils::log_stack_critical("dms telegram forcing reconnect", stack_used);
					self.state = State::Disconnected;
					self.client = None;
					return Ok(());
				}

				crate::utils::log_stack_usage("dms telegram before updates.next()");

				let update = match updates.next().await {
					Ok(u) => u,
					Err(e) => {
						error!("Error getting next update: {e}, reconnecting...");
						self.state = State::Disconnected;
						self.client = None;
						return Ok(());
					}
				};

				crate::utils::log_stack_usage("dms telegram after updates.next()");

				match update {
					Update::NewMessage(message) if !message.outgoing() => {
						let peer = match message.peer() {
							Ok(p) => p,
							Err(e) => {
								error!("Skipping message with unresolved peer: {e:?}");
								return Ok(());
							}
						};

						// Only process DMs (user peers)
						if !matches!(peer, grammers_client::types::Peer::User(_)) {
							return Ok(());
						}

						let text = message.text();
						let sender = match message.sender() {
							Some(s) => s,
							None => return Ok(()),
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
				Ok(())
			}
		}
	}

	async fn connect(&self) -> Result<(Client, UpdateStream)> {
		let session_filename = format!("{}_dm.session", self.config.telegram.username);
		let session_file = v_utils::xdg_state_file!(&session_filename);
		info!("Using session file: {}", session_file.display());

		info!("Opening session database");
		let session = match SqliteSession::open(&session_file) {
			Ok(s) => Arc::new(s),
			Err(e) => {
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

		info!("Connecting to Telegram with api_id: {}", self.config.telegram.api_id);
		let pool = SenderPool::new(Arc::clone(&session), self.config.telegram.api_id);
		let client = Client::new(&pool);
		let SenderPool { runner, updates, .. } = pool;
		tokio::spawn(runner.run());
		info!("Connected to Telegram");

		if !client.is_authorized().await? {
			info!("Not authorized, requesting login code for {}", self.config.telegram.phone);
			let token = client.request_login_code(&self.config.telegram.phone, &self.config.telegram.api_hash).await?;
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

			eprintln!("Session saved successfully");
			info!("Session saved successfully to {}", session_file.display());
		}

		// Pre-fetch all dialogs to warm the peer cache with access hashes.
		// This prevents "missing its hash" warnings when receiving updates for channels.
		info!("Pre-fetching dialogs to warm peer cache...");
		let mut dialog_count = 0;
		let mut dialogs = client.iter_dialogs();
		while let Some(dialog) = dialogs.next().await? {
			dialog_count += 1;
			debug!("Cached dialog: {} ({})", dialog.peer().name().unwrap_or_default(), dialog.peer().id());
		}
		info!("Cached {dialog_count} dialogs");

		let updates = client.stream_updates(
			updates,
			UpdatesConfiguration {
				catch_up: false,
				..Default::default()
			},
		);

		Ok((client, updates))
	}
}
