use std::{future::Future, path::Path, pin::Pin};

use clap::Args;
use color_eyre::eyre::{Context, ContextCompat, Result};
use google_gmail1::{Gmail, api::Message};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use regex::Regex;
use rustls;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument};
use v_utils::{elog, log};
use yup_oauth2::{ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod, authenticator_delegate::InstalledFlowDelegate};

// Wrapper to make yup-oauth2 Authenticator compatible with google-apis-common GetToken
#[derive(Clone)]
struct AuthWrapper(std::sync::Arc<yup_oauth2::authenticator::Authenticator<HttpsConnector<HttpConnector>>>);

impl google_apis_common::GetToken for AuthWrapper {
	fn get_token<'a>(&'a self, _scopes: &'a [&str]) -> Pin<Box<dyn Future<Output = Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>> {
		let auth = self.0.clone();
		Box::pin(async move {
			// Always use gmail.readonly scope regardless of what the API requests
			let scopes = &["https://www.googleapis.com/auth/gmail.readonly"];
			match auth.token(scopes).await {
				Ok(token) => {
					let access_token = token.token().map(|t| t.to_string());
					Ok(access_token)
				}
				Err(e) => Err(Box::new(e) as Box<dyn std::error::Error + Send + Sync>),
			}
		})
	}
}

use crate::{
	config::{AppConfig, EmailConfig},
	db::Database,
	telegram_notifier::TelegramNotifier,
};

// Custom flow delegate to print a nice URL with tmux link support
struct CustomFlowDelegate;

impl InstalledFlowDelegate for CustomFlowDelegate {
	fn present_user_url<'a>(&'a self, url: &'a str, need_code: bool) -> Pin<Box<dyn Future<Output = std::result::Result<String, String>> + Send + 'a>> {
		Box::pin(async move {
			if need_code {
				// Print URL with OSC 8 hyperlink for terminal support
				println!("\n\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\n", url, url);

				// Read code from stdin
				use std::io::{self, BufRead};
				let mut code = String::new();
				io::stdin().lock().read_line(&mut code).map_err(|e| e.to_string())?;
				Ok(code.trim().to_string())
			} else {
				// HTTP redirect method - just print the URL
				println!("\n\x1b]8;;{}\x1b\\{}\x1b]8;;\x1b\\\n", url, url);
				Ok(String::new())
			}
		})
	}
}

#[derive(Args)]
pub struct EmailArgs {}

pub fn main(config: AppConfig, _args: EmailArgs) -> Result<()> {
	// Install default crypto provider for rustls
	let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

	// Set up tracing with file logging (truncate old logs)
	let log_file = v_utils::xdg_state_file!("email.log");
	if log_file.exists() {
		std::fs::remove_file(&log_file)?;
	}
	let file_appender = tracing_appender::rolling::never(log_file.parent().unwrap(), log_file.file_name().unwrap());
	let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

	tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_max_level(tracing::Level::DEBUG).init();

	let email_config = config.email.context("Email config not found in config file")?;
	let notifier = TelegramNotifier::new(config.telegram.clone());
	let db = Database::new(&config.clickhouse);

	println!("Email: Listening...");
	info!("Monitoring email: {}", email_config.email);

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_email_monitor(&email_config, &notifier, &db).await {
				error!("Email monitor error: {e}");
				error!("Retrying in 5 minutes...");
				time::sleep(Duration::from_secs(5 * 60)).await;
			} else {
				// Successfully checked - wait before next check
				time::sleep(Duration::from_secs(60)).await;
			}
		}
	})
}

#[instrument(skip_all)]
async fn run_email_monitor(config: &EmailConfig, notifier: &TelegramNotifier, db: &Database) -> Result<()> {
	let monitor = EmailMonitor::new(config.clone(), notifier.clone(), db.clone())?;
	monitor.run().await
}

#[derive(Clone)]
pub struct EmailMonitor {
	config: EmailConfig,
	notifier: TelegramNotifier,
	db: Database,
	ignore_regexes: Vec<Regex>,
}

impl std::fmt::Debug for EmailMonitor {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("EmailMonitor")
			.field("config", &self.config)
			.field("notifier", &self.notifier)
			.field("db", &self.db)
			.finish()
	}
}

impl EmailMonitor {
	pub fn new(config: EmailConfig, notifier: TelegramNotifier, db: Database) -> Result<Self> {
		let ignore_regexes = config
			.ignore_patterns
			.iter()
			.map(|pattern| Regex::new(pattern).context(format!("Invalid ignore pattern: {}", pattern)))
			.collect::<Result<Vec<_>>>()?;

		Ok(Self {
			config,
			notifier,
			db,
			ignore_regexes,
		})
	}

	/// Main entry point to start monitoring emails
	#[instrument(skip_all)]
	pub async fn run(&self) -> Result<()> {
		info!("Starting email monitor");

		let hub = self.create_gmail_hub().await?;

		// Fetch unread messages (this triggers the OAuth flow if needed)
		let messages = self.fetch_unread_messages(&hub).await?;

		log!("Successfully authenticated with Gmail API");
		info!("Found {} unread messages", messages.len());

		for message in messages {
			if let Err(e) = self.process_message(&hub, &message).await {
				tracing::error!("Failed to process message: {}", e);
			}
		}

		Ok(())
	}

	/// Create Gmail API hub with authentication
	/// On first run, this will open a browser for OAuth2 authentication
	/// Subsequent runs will use the saved token
	#[instrument(skip_all)]
	async fn create_gmail_hub(&self) -> Result<Gmail<HttpsConnector<HttpConnector>>> {
		info!("Authenticating with Gmail API...");

		// Create OAuth2 application secret
		let secret = ApplicationSecret {
			client_id: self.config.client_id.clone(),
			client_secret: self.config.client_secret.clone(),
			auth_uri: "https://accounts.google.com/o/oauth2/auth".to_string(),
			token_uri: "https://oauth2.googleapis.com/token".to_string(),
			..Default::default()
		};

		// Build authenticator with token persistence and custom URL printer
		let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
			.persist_tokens_to_disk(Path::new(&self.config.token_path))
			.flow_delegate(Box::new(CustomFlowDelegate))
			.build()
			.await
			.context("Failed to create authenticator")?;

		let https = HttpsConnectorBuilder::new()
			.with_native_roots()
			.context("Failed to load native roots")?
			.https_or_http()
			.enable_http1()
			.build();

		let client = Client::builder(hyper_util::rt::TokioExecutor::new()).build(https);

		// Wrap authenticator to make it compatible with Gmail API
		let auth_wrapper = AuthWrapper(std::sync::Arc::new(auth));

		Ok(Gmail::new(client, auth_wrapper))
	}

	/// Fetch unread messages from Gmail
	#[instrument(skip(self, hub))]
	async fn fetch_unread_messages(&self, hub: &Gmail<HttpsConnector<HttpConnector>>) -> Result<Vec<Message>> {
		let result = hub
			.users()
			.messages_list(&self.config.email)
			.q("is:unread")
			.doit()
			.await
			.map_err(|e| color_eyre::eyre::eyre!("Failed to fetch messages: {:#?}", e))?;

		let mut messages = Vec::new();
		if let Some(msg_list) = result.1.messages {
			for msg in msg_list {
				if let Some(ref id) = msg.id {
					let (_, full_message) = hub
						.users()
						.messages_get(&self.config.email, id)
						.format("full")
						.doit()
						.await
						.context("Failed to fetch message details")?;
					messages.push(full_message);
				}
			}
		}

		Ok(messages)
	}

	/// Process a single message
	#[instrument(skip(self, hub, message))]
	async fn process_message(&self, hub: &Gmail<HttpsConnector<HttpConnector>>, message: &Message) -> Result<()> {
		let message_id = message.id.as_ref().wrap_err("Message has no ID")?;

		// Check if already processed
		if self.db.is_email_processed(message_id).await? {
			debug!("Message {} already processed, skipping", message_id);
			return Ok(());
		}

		debug!("Processing message: {}", message_id);

		// Extract message details
		let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());
		let subject = self.extract_header(message, "Subject").unwrap_or_else(|| "No Subject".to_string());
		let snippet = message.snippet.as_deref().unwrap_or("");

		debug!("From: {}, Subject: {}", from, subject);

		// Check if sender matches ignore patterns
		if self.should_ignore(&from) {
			debug!("Ignoring email from: {} (matches ignore pattern)", from);
			return Ok(());
		}

		// Check if email is from a human using AI
		let is_from_human = self.eval_is_human(message).await?;

		if is_from_human {
			self.forward_to_telegram(&from, &subject, snippet).await?;
			log!("Forwarded human email from: {}", from);
		} else {
			// Mark as read if not from human (automated emails, etc.)
			self.mark_as_read(hub, message_id).await?;
			elog!("Marked non-human email as read: {}", from);
		}

		// Mark as processed in database
		self.db.mark_email_processed(message_id, &from, &subject, is_from_human).await?;

		Ok(())
	}

	/// Extract header value from message
	fn extract_header(&self, message: &Message, header_name: &str) -> Option<String> {
		message.payload.as_ref()?.headers.as_ref()?.iter().find(|h| h.name.as_deref() == Some(header_name))?.value.clone()
	}

	/// Check if sender should be ignored based on configured patterns
	fn should_ignore(&self, from: &str) -> bool {
		self.ignore_regexes.iter().any(|regex| regex.is_match(from))
	}

	/// Forward email to Telegram Alerts channel
	#[instrument(skip(self, body))]
	async fn forward_to_telegram(&self, from: &str, subject: &str, body: &str) -> Result<()> {
		let text = format!("📧 New Email\n\nFrom: {}\nSubject: {}\n\n{}", from, subject, body);

		self.notifier.send_message_to_alerts(&text).await?;
		info!("Forwarded email from {} to Telegram", from);

		Ok(())
	}

	/// Mark message as read
	#[instrument(skip(self, hub))]
	async fn mark_as_read(&self, hub: &Gmail<HttpsConnector<HttpConnector>>, message_id: &str) -> Result<()> {
		use google_gmail1::api::ModifyMessageRequest;

		let request = ModifyMessageRequest {
			remove_label_ids: Some(vec!["UNREAD".to_string()]),
			..Default::default()
		};

		hub.users()
			.messages_modify(request, &self.config.email, message_id)
			.doit()
			.await
			.context("Failed to mark message as read")?;

		Ok(())
	}

	/// Evaluate if email is from a human using AI
	/// This will be similar to perf-eval in ~/s/todo
	async fn eval_is_human(&self, _message: &Message) -> Result<bool> {
		// TODO: Implement AI-based detection using ask_llm crate
		// Similar pattern to perf-eval in ~/s/todo
		// Analyze:
		// - Email formatting and structure
		// - Sender patterns and metadata
		// - Content personalization
		// - Header analysis

		// For now, return true for all messages (forward everything)
		Ok(true)
	}
}
