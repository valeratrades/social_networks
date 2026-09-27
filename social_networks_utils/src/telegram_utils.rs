//! Shared utilities for Telegram MTProto connections.
//!
//! Provides structured concurrency patterns for grammers client usage.

use std::{future::Future, io::IsTerminal as _, pin::Pin, sync::Arc};

use color_eyre::eyre::{Result, bail};
use futures::future::{Either, select};
use grammers_client::{Client, SignInError, client::UpdatesConfiguration};
use grammers_mtsender::SenderPool;
use tracing::{debug, error, info};

use crate::db::{Database, DbSession};

/// A pinned future representing the MTProto runner.
/// Store this in your state and poll it alongside other futures using `select`.
pub type RunnerFuture = Pin<Box<dyn Future<Output = ()> + Send>>;

/// Result of establishing a Telegram connection.
pub struct TelegramConnection {
	pub client: Client,
	pub updates: grammers_client::client::UpdateStream,
	pub runner: RunnerFuture,
	/// `Client` keeps its session private, but updates that carry a bare peer id can only be
	/// named by looking that id up in the cache the dialog prefetch below warms.
	pub session: Arc<DbSession>,
}

/// Configuration for establishing a Telegram connection.
pub struct ConnectionConfig<'a> {
	pub username: &'a str,
	pub phone: &'a str,
	pub api_id: i32,
	pub api_hash: &'a str,
	/// Session name suffix (e.g., "_dm" for DM monitor, "" for main)
	pub session_suffix: &'a str,
	/// Suffix of the session to copy from when this one does not exist yet. The auth key travels
	/// with the copy, so it is authorized without a login code and without registering a new device.
	pub seed_from: Option<&'a str>,
}

/// Establishes a Telegram connection with proper session handling.
///
/// This handles:
/// - Session load from the db (importing or seeding one on first use)
/// - Authentication (including 2FA)
/// - Dialog pre-fetching for peer cache warming
///
/// Returns a `TelegramConnection` with the runner as a pinned future for structured concurrency.
/// The caller should use `select` to poll the runner alongside their main logic.
pub async fn connect(config: ConnectionConfig<'_>) -> Result<TelegramConnection> {
	let name = format!("{}{}", config.username, config.session_suffix);
	let seed = config.seed_from.map(|seed| format!("{}{seed}", config.username));
	let session = Arc::new(DbSession::open(Database::try_new().await?, &name, seed.as_deref()).await?);

	info!("Connecting to Telegram with api_id: {}", config.api_id);
	let pool = SenderPool::new(Arc::clone(&session), config.api_id);
	let SenderPool { runner, handle, updates } = pool;
	let client = Client::new(handle);
	let mut runner: RunnerFuture = Box::pin(runner.run());

	// The pool opens no socket until its runner is polled, so every RPC in the handshake
	// deadlocks unless the runner is driven alongside it.
	let handshake = async {
		if !client.is_authorized().await? {
			authenticate(&client, config.phone, config.api_hash, &name).await?;
		}
		info!("Connected to Telegram");

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

		client
			.stream_updates(
				updates,
				UpdatesConfiguration {
					catch_up: false,
					..Default::default()
				},
			)
			.await
			.map_err(|e| color_eyre::eyre::eyre!(e))
	};

	let updates = match select(std::pin::pin!(handshake), runner.as_mut()).await {
		Either::Left((result, _)) => result?,
		// Braced so the macro lands in statement position: its expansion ends in a `;`, which
		// `semicolon_in_expressions_from_non_local_macros` rejects directly under a match arm.
		Either::Right(((), _)) => {
			bail!("MTProto runner exited during connect");
		}
	};

	Ok(TelegramConnection { client, updates, runner, session })
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
async fn authenticate(client: &Client, phone: &str, api_hash: &str, session: &str) -> Result<()> {
	// a daemon restarted by its supervisor would otherwise have a code sent to the phone on every restart
	if !std::io::stdin().is_terminal() {
		bail!("Sign in failed: telegram session {session} is not authorized, and there is no terminal to type a login code into");
	}
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
	info!("Telegram session {session} authorized");
	Ok(())
}
