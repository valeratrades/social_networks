//! Shared utilities for Telegram MTProto connections.
//!
//! Provides structured concurrency patterns for grammers client usage.

use std::{future::Future, pin::Pin, sync::Arc};

use color_eyre::eyre::{Result, bail};
use grammers_client::{Client, SignInError, UpdatesConfiguration};
use grammers_mtsender::SenderPool;
use grammers_session::storages::SqliteSession;
use tracing::{debug, error, info};

/// A pinned future representing the MTProto runner.
/// Store this in your state and poll it alongside other futures using `select`.
pub type RunnerFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Result of establishing a Telegram connection.
pub struct TelegramConnection {
	pub client: Client,
	pub updates: grammers_client::client::updates::UpdateStream,
	pub runner: RunnerFuture,
}

/// Configuration for establishing a Telegram connection.
pub struct ConnectionConfig<'a> {
	pub username: &'a str,
	pub phone: &'a str,
	pub api_id: i32,
	pub api_hash: &'a str,
	/// Session file suffix (e.g., "_dm" for DM monitor, "" for main)
	pub session_suffix: &'a str,
}

/// Establishes a Telegram connection with proper session handling.
///
/// This handles:
/// - Session file creation/corruption recovery
/// - Authentication (including 2FA)
/// - Dialog pre-fetching for peer cache warming
///
/// Returns a `TelegramConnection` with the runner as a pinned future for structured concurrency.
/// The caller should use `select` to poll the runner alongside their main logic.
pub async fn connect(config: ConnectionConfig<'_>) -> Result<TelegramConnection> {
	let session_filename = format!("{}{}.session", config.username, config.session_suffix);
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

	info!("Connecting to Telegram with api_id: {}", config.api_id);
	let pool = SenderPool::new(Arc::clone(&session), config.api_id);
	let client = Client::new(&pool);
	let SenderPool { runner, updates, .. } = pool;
	let runner: RunnerFuture = Box::pin(runner.run());
	info!("Connected to Telegram");

	if !client.is_authorized().await? {
		authenticate(&client, config.phone, config.api_hash, &session_file).await?;
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

	Ok(TelegramConnection { client, updates, runner })
}

/// Check stack usage and return true if we should force a reconnect.
/// Logs a critical warning if stack usage is above threshold.
pub fn should_reconnect_for_stack() -> bool {
	let (stack_used, _) = crate::utils::stack_usage();
	if stack_used > 6 * 1024 * 1024 {
		crate::utils::log_stack_critical("telegram forcing reconnect", stack_used);
		return true;
	}
	false
}
/// Log current stack usage for monitoring accumulation.
pub fn log_stack(context: &str) {
	crate::utils::log_stack_usage(context);
}
async fn authenticate(client: &Client, phone: &str, api_hash: &str, session_file: &std::path::Path) -> Result<()> {
	info!("Not authorized, requesting login code for {phone}");
	let token = client.request_login_code(phone, api_hash).await?;
	info!("Login code requested successfully, check your Telegram app");

	println!("Enter the code you received: ");
	let mut code = String::new();
	std::io::stdin().read_line(&mut code)?;
	let code = code.trim();
	println!("Code received, authenticating...");
	info!("Received code from user (length: {})", code.len());
	debug!("Code value: '{code}'");

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
			bail!("Sign in failed: {e}");
		}
	}

	eprintln!("Session saved successfully");
	info!("Session saved successfully to {}", session_file.display());
	Ok(())
}
