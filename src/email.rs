use std::{future::Future, path::Path, pin::Pin};

use clap::Args;
use color_eyre::eyre::{Context, ContextCompat, Result};
use google_gmail1::{Gmail, api::Message};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use regex::Regex;
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
			// Use gmail.modify scope to allow marking messages as read
			let scopes = &["https://www.googleapis.com/auth/gmail.modify"];
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
pub struct EmailArgs {
	/// Mark all unread emails as read without processing
	#[arg(long)]
	mark_all_read: bool,
}

pub fn main(config: AppConfig, args: EmailArgs) -> Result<()> {
	v_utils::clientside!("email");

	// Install default crypto provider for rustls
	let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

	let email_config = config.email.context("Email config not found in config file")?;
	let notifier = TelegramNotifier::new(config.telegram.clone());
	let db = Database::new(&config.clickhouse);

	let runtime = tokio::runtime::Runtime::new()?;

	// Handle mark-all-read mode
	if args.mark_all_read {
		return runtime.block_on(async {
			let monitor = EmailMonitor::new(email_config.clone(), notifier.clone(), db.clone())?;
			let hub = monitor.create_gmail_hub().await?;
			monitor.mark_all_as_read(&hub).await
		});
	}

	println!("Email: Listening...");
	info!("Monitoring email: {}", email_config.email);

	runtime.block_on(async {
		// Run database migrations to ensure schema exists
		db.migrate().await.context("Failed to run database migrations")?;

		// Create the EmailMonitor once
		let monitor = EmailMonitor::new(email_config.clone(), notifier.clone(), db.clone())?;

		// Create Gmail hub once and reuse it
		let hub = match monitor.create_gmail_hub().await {
			Ok(hub) => {
				log!("Successfully authenticated with Gmail API");
				hub
			}
			Err(e) => {
				error!("Failed to create Gmail hub: {e}");
				return Err(e);
			}
		};

		loop {
			if let Err(e) = monitor.run_with_hub(&hub).await {
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

	/// Main entry point to start monitoring emails with a pre-created hub
	#[instrument(skip_all)]
	pub async fn run_with_hub(&self, hub: &Gmail<HttpsConnector<HttpConnector>>) -> Result<()> {
		info!("Starting email monitor");

		// Fetch unread messages
		let messages = self.fetch_unread_messages(hub).await?;

		info!("Found {} unread messages", messages.len());

		for message in messages {
			if let Err(e) = self.process_message(hub, &message).await {
				let message_id = message.id.as_ref().map(|s| s.as_str()).unwrap_or("unknown");
				let from = self.extract_header(&message, "From").unwrap_or_else(|| "Unknown".to_string());
				tracing::error!("Failed to process message {} from {}: {:#}", message_id, from, e);
			}
		}

		Ok(())
	}

	/// Create Gmail API hub with authentication
	/// On first run, this will open a browser for OAuth2 authentication
	/// Subsequent runs will use the saved token
	#[instrument(skip_all)]
	pub async fn create_gmail_hub(&self) -> Result<Gmail<HttpsConnector<HttpConnector>>> {
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

	/// Fetch unread messages from Gmail with pagination
	#[instrument(skip(self, hub))]
	async fn fetch_unread_messages(&self, hub: &Gmail<HttpsConnector<HttpConnector>>) -> Result<Vec<Message>> {
		let mut all_messages = Vec::new();
		let mut page_token: Option<String> = None;

		loop {
			let mut request = hub.users().messages_list(&self.config.email).q("is:unread").max_results(500); // Fetch up to 500 message IDs at a time

			if let Some(ref token) = page_token {
				request = request.page_token(token);
			}

			let result = request.doit().await.map_err(|e| color_eyre::eyre::eyre!("Failed to fetch messages: {:#?}", e))?;

			if let Some(msg_list) = result.1.messages {
				// Fetch all message details concurrently
				use futures::stream::{self, StreamExt};

				let messages: Vec<_> = stream::iter(msg_list.iter())
					.map(|msg| async {
						if let Some(ref id) = msg.id {
							let result = hub
								.users()
								.messages_get(&self.config.email, id)
								.format("full")
								.doit()
								.await;
							Some(result)
						} else {
							None
						}
					})
					.buffer_unordered(50) // Fetch up to 50 messages concurrently
					.collect()
					.await;

				for msg_result in messages {
					if let Some(result) = msg_result {
						let (_, full_message) = result.context("Failed to fetch message details")?;
						all_messages.push(full_message);
					}
				}
			}

			// Check if there are more pages
			page_token = result.1.next_page_token;
			if page_token.is_none() {
				break;
			}
		}

		Ok(all_messages)
	}

	/// Process a single message
	#[instrument(skip(self, hub, message))]
	async fn process_message(&self, hub: &Gmail<HttpsConnector<HttpConnector>>, message: &Message) -> Result<()> {
		let message_id = message.id.as_ref().wrap_err("Message has no ID")?;

		debug!("[STEP 1] Checking if message {} is already processed", message_id);
		// Check if already processed
		match self.db.is_email_processed(message_id).await {
			Ok(true) => {
				debug!("Message {} already processed, skipping", message_id);
				return Ok(());
			}
			Ok(false) => {
				debug!("[STEP 1] Message {} not processed yet", message_id);
			}
			Err(e) => {
				error!("[STEP 1] Error checking if message {} is processed: {:#}", message_id, e);
				return Err(e);
			}
		}

		debug!("[STEP 2] Processing message: {}", message_id);

		// Extract message details
		let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());
		let subject = self.extract_header(message, "Subject").unwrap_or_else(|| "No Subject".to_string());
		let snippet = message.snippet.as_deref().unwrap_or("");

		debug!("[STEP 3] From: {}, Subject: {}", from, subject);

		// Check if sender matches ignore patterns
		if self.should_ignore(&from) {
			debug!("Ignoring email from: {} (matches ignore pattern)", from);
			return Ok(());
		}

		debug!("[STEP 4] Calling eval_is_human for {}", from);
		// Check if email is from a human using AI
		let is_from_human = match self.eval_is_human(message).await {
			Ok(result) => {
				debug!("[STEP 4] eval_is_human returned: {}", result);
				result
			}
			Err(e) => {
				error!("[STEP 4] Error in eval_is_human for {}: {:#}", from, e);
				return Err(e);
			}
		};

		if is_from_human {
			// Forward to Telegram and mark as processed internally
			// Do NOT mark as read in Gmail - keep visible in inbox
			self.forward_to_telegram(&from, &subject, snippet).await?;
			log!("Forwarded human email from: {}", from);

			// Mark as processed in DB so we don't reprocess it
			self.db.mark_email_processed(message_id, &from, &subject, is_from_human).await?;
		} else {
			// Mark as read if not from human (automated emails, etc.)
			self.mark_as_read(hub, message_id).await?;
			elog!("Marked non-human email as read: {}", from);

			// Mark as processed in DB
			self.db.mark_email_processed(message_id, &from, &subject, is_from_human).await?;
		}

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

	/// Mark all unread messages as read (processes in batches with concurrency)
	#[instrument(skip(self, hub))]
	pub async fn mark_all_as_read(&self, hub: &Gmail<HttpsConnector<HttpConnector>>) -> Result<()> {
		const BATCH_SIZE: usize = 100;
		const CONCURRENT_REQUESTS: usize = 20;

		let mut total_marked = 0;

		loop {
			println!("Fetching next batch of unread messages...");
			let messages = self.fetch_unread_messages(hub).await?;

			let count = messages.len();
			if count == 0 {
				if total_marked == 0 {
					println!("No unread messages found.");
				} else {
					println!("\nAll done! Marked {} total messages as read.", total_marked);
				}
				return Ok(());
			}

			// Process only up to BATCH_SIZE messages at a time
			let batch_to_process = std::cmp::min(count, BATCH_SIZE);
			let batch = &messages[..batch_to_process];

			println!(
				"Marking {} unread messages as read (batch {} with concurrency {})...",
				batch_to_process,
				total_marked / BATCH_SIZE + 1,
				CONCURRENT_REQUESTS
			);

			// Process messages concurrently and print results as they complete
			use std::sync::{
				Arc,
				atomic::{AtomicUsize, Ordering},
			};

			use futures::stream::{self, StreamExt};

			let batch_marked = Arc::new(AtomicUsize::new(0));
			let batch_marked_clone = batch_marked.clone();
			let base_count = total_marked;

			stream::iter(batch.iter())
				.for_each_concurrent(CONCURRENT_REQUESTS, |message| {
					let batch_marked = batch_marked_clone.clone();
					async move {
						let message_id = message.id.clone().unwrap_or_default();
						let result = if !message_id.is_empty() {
							self.mark_as_read(hub, &message_id).await
						} else {
							Err(color_eyre::eyre::eyre!("Message has no ID"))
						};
						let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());

						// Print immediately as each request completes
						match &result {
							Ok(_) => {
								let current = batch_marked.fetch_add(1, Ordering::SeqCst) + 1;
								println!("[{}/{}] Marked as read: {}", base_count + current, base_count + batch_to_process, from);
							}
							Err(e) => {
								error!("Failed to mark message {} as read: {:#}", message_id, e);
								eprintln!("Error marking message {} as read: {}", message_id, e);
							}
						}
					}
				})
				.await;

			total_marked += batch_marked.load(Ordering::SeqCst);

			// If we processed fewer than BATCH_SIZE messages, we're done
			if count < BATCH_SIZE {
				println!("\nAll done! Marked {} total messages as read.", total_marked);
				return Ok(());
			}
		}
	}

	/// Evaluate if email is from a human using AI
	async fn eval_is_human(&self, message: &Message) -> Result<bool> {
		// Set CLAUDE_TOKEN from config if provided
		if let Some(ref token) = self.config.claude_token {
			unsafe {
				std::env::set_var("CLAUDE_TOKEN", token);
			}
		}
		// Extract email information
		let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());
		let subject = self.extract_header(message, "Subject").unwrap_or_else(|| "No Subject".to_string());
		let date = self.extract_header(message, "Date").unwrap_or_else(|| "Unknown".to_string());
		let reply_to = self.extract_header(message, "Reply-To");
		let list_unsubscribe = self.extract_header(message, "List-Unsubscribe");

		// Get email body (snippet or full body if available)
		let body = message.snippet.as_deref().unwrap_or("");

		// Get some headers to analyze
		let headers_info = if let Some(payload) = &message.payload {
			if let Some(headers) = &payload.headers {
				headers
					.iter()
					.filter(|h| {
						// Include relevant headers for analysis
						matches!(
							h.name.as_deref(),
							Some("X-Mailer") | Some("User-Agent") | Some("X-Auto-Response-Suppress") | Some("Auto-Submitted") | Some("Precedence")
						)
					})
					.filter_map(|h| {
						let name = h.name.as_deref()?;
						let value = h.value.as_deref()?;
						Some(format!("{}: {}", name, value))
					})
					.collect::<Vec<_>>()
					.join("\n")
			} else {
				String::new()
			}
		} else {
			String::new()
		};

		// Build prompt for LLM
		let prompt = format!(
			r#"Analyze this email and determine if it's from a human or an automated system.

From: {}
Subject: {}
Date: {}
Reply-To: {}
List-Unsubscribe: {}

Additional Headers:
{}

Body Preview:
{}

Consider these factors:
1. Marketing emails, newsletters, automated notifications should be marked as NOT human
2. Personal emails with conversational tone should be marked as human
3. Presence of unsubscribe links typically indicates automated email
4. Generic greetings like "Dear valued customer" indicate automation
5. Personal salutations and informal language indicate human
6. Auto-Submitted or X-Auto-Response-Suppress headers indicate automation

Respond with ONLY "yes" if from a human or "no" if automated/marketing. No explanation."#,
			from,
			subject,
			date,
			reply_to.as_deref().unwrap_or("N/A"),
			list_unsubscribe.as_deref().unwrap_or("N/A"),
			if headers_info.is_empty() { "None" } else { &headers_info },
			body
		);

		// Call LLM using ask_llm crate
		debug!("Calling LLM for email from: {}", from);
		let response = match ask_llm::oneshot(&prompt, ask_llm::Model::Fast).await {
			Ok(r) => r,
			Err(e) => {
				error!("LLM call failed for email from {}: {:#}", from, e);
				return Err(e).context("Failed to call LLM for email evaluation. Make sure CLAUDE_TOKEN environment variable is set");
			}
		};

		// Parse response
		let is_human = response.text.trim().to_lowercase().starts_with("yes");

		debug!(
			"LLM evaluation for email from {}: {} (cost: {:.4} cents)",
			from,
			if is_human { "HUMAN" } else { "AUTOMATED" },
			response.cost_cents
		);

		Ok(is_human)
	}
}
