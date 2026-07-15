use std::{convert::Infallible, future::Future, path::Path, pin::Pin, sync::Arc};

use clap::Args;
use color_eyre::eyre::{Context, ContextCompat, Result};
use google_gmail1::{Gmail, api::Message};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use imap::{ImapConnection, Session};
use regex::Regex;
use serde::{Deserialize, Serialize};
use social_networks_utils::db::Database;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument};
use v_utils::{elog, log, macros::MyConfigPrimitives};
use yup_oauth2::{ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod, authenticator_delegate::InstalledFlowDelegate};

use crate::{
	client::{AdapterError, Client as AdapterClient},
	telegram_dms::TelegramConfig,
	telegram_notifier::TelegramNotifier,
};

const SURFACE: &str = "email";
#[derive(Args)]
pub struct EmailArgs {
	/// Mark all unread emails as read without processing
	#[arg(long)]
	pub mark_all_read: bool,
}
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct EmailConfig {
	/// Gmail email address to monitor
	pub email: String,
	/// Authentication method (IMAP or OAuth)
	#[primitives(skip)]
	pub auth: EmailAuth,
	/// Regex patterns to match against sender email to ignore (skip processing entirely)
	#[serde(default)]
	#[primitives(skip)]
	pub ignore_patterns: Vec<String>,
	/// Patterns that mark an email as alert-worthy without LLM evaluation
	#[serde(default)]
	#[primitives(skip)]
	pub important_if_contains: ImportantIfContains,
	/// Claude API token for LLM-based email classification (optional, falls back to CLAUDE_TOKEN env var)
	#[serde(default)]
	pub claude_token: Option<String>,
}

/// Patterns to check for marking email as alert-worthy.
/// If any pattern matches, the email is forwarded without LLM check.
/// Top-level `any` matches against all fields (subject, body, address).
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct ImportantIfContains {
	/// Patterns to match against any field (subject, body, address)
	#[serde(default)]
	pub any: Vec<String>,
	/// Patterns to match against subject/title only
	#[serde(default)]
	pub subject: Vec<String>,
	/// Patterns to match against body only
	#[serde(default)]
	pub body: Vec<String>,
	/// Patterns to match against sender address only
	#[serde(default)]
	pub address: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EmailAuth {
	Imap(ImapAuth),
	Oauth(OAuthAuth),
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct ImapAuth {
	pub pass: String,
}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct OAuthAuth {
	pub client_id: String,
	pub client_secret: String,
	/// Path to store auth tokens (default: ~/.local/state/social_networks/gmail_tokens.json)
	#[serde(default = "__default_email_token_path")]
	#[primitives(skip)]
	pub token_path: String,
}

#[derive(Clone)]
pub struct EmailMonitor {
	config: EmailConfig,
	notifier: TelegramNotifier,
	db: Database,
	ignore_regexes: Vec<Regex>,
}
impl EmailMonitor {
	pub fn try_new(config: EmailConfig, notifier: TelegramNotifier, db: Database) -> Result<Self> {
		let ignore_regexes = config
			.ignore_patterns
			.iter()
			.map(|pattern| Regex::new(pattern).context(format!("Invalid ignore pattern: {pattern}")))
			.collect::<Result<Vec<_>>>()?;

		Ok(Self {
			config,
			notifier,
			db,
			ignore_regexes,
		})
	}

	pub async fn try_from_configs(email_config: EmailConfig, telegram_config: TelegramConfig) -> Result<Self> {
		// Install default crypto provider for rustls (needed for OAuth)
		let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
		let notifier = TelegramNotifier::new(telegram_config);
		let db = Database::try_new().await.context("Failed to open database")?;
		Self::try_new(email_config, notifier, db)
	}

	/// Main entry point - dispatches to IMAP or OAuth based on config
	#[instrument(skip_all)]
	pub async fn run(&self) -> Result<()> {
		info!("Starting email monitor");

		match &self.config.auth {
			EmailAuth::Imap(_) => self.run_imap().await,
			EmailAuth::Oauth(oauth) => self.run_oauth(oauth).await,
		}
	}

	/// Mark all as read - dispatches to IMAP or OAuth based on config
	pub async fn mark_all_as_read(&self) -> Result<()> {
		match &self.config.auth {
			EmailAuth::Imap(_) => self.mark_all_as_read_imap().await,
			EmailAuth::Oauth(oauth) => self.mark_all_as_read_oauth(oauth).await,
		}
	}

	// ==================== IMAP Implementation ====================

	fn connect_imap(&self) -> Result<Session<Box<dyn ImapConnection>>> {
		let pass = match &self.config.auth {
			EmailAuth::Imap(imap_auth) => &imap_auth.pass,
			EmailAuth::Oauth(_) => unreachable!(),
		};

		let client = imap::ClientBuilder::new("imap.gmail.com", 993).connect().context("Failed to connect to Gmail IMAP")?;

		let session = client.login(&self.config.email, pass).map_err(|e| color_eyre::eyre::eyre!("IMAP login failed: {:?}", e.0))?;

		Ok(session)
	}

	async fn run_imap(&self) -> Result<()> {
		let this = self.clone();

		tokio::task::spawn_blocking(move || {
			let mut session = this.connect_imap()?;
			session.select("INBOX").context("Failed to select INBOX")?;

			let uids = session.uid_search("UNSEEN").context("Failed to search for unread messages")?;
			info!("Found {} unread messages", uids.len());

			for uid in uids.iter() {
				if let Err(e) = this.process_message_imap(&mut session, *uid) {
					error!("Failed to process message {uid}: {e:#}");
				}
			}

			session.logout().ok();
			Ok(())
		})
		.await?
	}

	fn process_message_imap(&self, session: &mut Session<Box<dyn ImapConnection>>, uid: u32) -> Result<()> {
		let messages = session.uid_fetch(uid.to_string(), "(UID ENVELOPE BODY.PEEK[])").context("Failed to fetch message")?;

		let message = messages.iter().next().context("Message not found")?;
		let envelope = message.envelope().context("No envelope")?;

		let from = envelope
			.from
			.as_ref()
			.and_then(|addrs| addrs.first())
			.map(|addr| {
				let name = addr.name.as_ref().map(|n| String::from_utf8_lossy(n).to_string()).unwrap_or_default();
				let mailbox = addr.mailbox.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();
				let host = addr.host.as_ref().map(|h| String::from_utf8_lossy(h).to_string()).unwrap_or_default();
				if name.is_empty() {
					format!("{mailbox}@{host}")
				} else {
					format!("{name} <{mailbox}@{host}>")
				}
			})
			.unwrap_or_else(|| "Unknown".to_string());

		let subject = envelope
			.subject
			.as_ref()
			.map(|s| String::from_utf8_lossy(s).to_string())
			.unwrap_or_else(|| "No Subject".to_string());

		let date = envelope.date.as_ref().map(|d| String::from_utf8_lossy(d).to_string()).unwrap_or_else(|| "Unknown".to_string());

		let body_preview: String = message.body().map(decode_body_preview).unwrap_or_default();

		let email_msg = EmailMessage {
			id: format!("imap-{uid}"),
			from,
			subject,
			date,
			body_preview,
			reply_to: None,
			list_unsubscribe: None,
			extra_headers: String::new(),
		};

		self.process_email_common(&email_msg, |_| self.mark_as_read_imap(session, uid))
	}

	fn mark_as_read_imap(&self, session: &mut Session<Box<dyn ImapConnection>>, uid: u32) -> Result<()> {
		session.uid_store(uid.to_string(), "+FLAGS (\\Seen)").context("Failed to mark message as read")?;
		Ok(())
	}

	async fn mark_all_as_read_imap(&self) -> Result<()> {
		let this = self.clone();

		tokio::task::spawn_blocking(move || {
			let mut session = this.connect_imap()?;
			session.select("INBOX").context("Failed to select INBOX")?;

			let uids = session.uid_search("UNSEEN").context("Failed to search for unread messages")?;
			let count = uids.len();

			if count == 0 {
				println!("No unread messages found.");
				return Ok(());
			}

			println!("Marking {count} unread messages as read...");

			for (i, uid) in uids.iter().enumerate() {
				let from = if let Ok(messages) = session.uid_fetch(uid.to_string(), "ENVELOPE") {
					messages
						.iter()
						.next()
						.and_then(|m| m.envelope())
						.and_then(|e| e.from.as_ref())
						.and_then(|addrs| addrs.first())
						.map(|addr| {
							let name = addr.name.as_ref().map(|n| String::from_utf8_lossy(n).to_string()).unwrap_or_default();
							let mailbox = addr.mailbox.as_ref().map(|m| String::from_utf8_lossy(m).to_string()).unwrap_or_default();
							let host = addr.host.as_ref().map(|h| String::from_utf8_lossy(h).to_string()).unwrap_or_default();
							if name.is_empty() {
								format!("{mailbox}@{host}")
							} else {
								format!("{name} <{mailbox}@{host}>")
							}
						})
						.unwrap_or_else(|| "Unknown".to_string())
				} else {
					"Unknown".to_string()
				};

				this.mark_as_read_imap(&mut session, *uid)?;
				println!("[{}/{}] Marked as read: {}", i + 1, count, from);
			}

			println!("\nAll done! Marked {count} messages as read.");
			session.logout().ok();
			Ok(())
		})
		.await?
	}

	// ==================== OAuth/Gmail API Implementation ====================

	async fn create_gmail_hub(&self, oauth: &OAuthAuth) -> Result<Gmail<HttpsConnector<HttpConnector>>> {
		info!("Authenticating with Gmail API...");

		let secret = ApplicationSecret {
			client_id: oauth.client_id.clone(),
			client_secret: oauth.client_secret.clone(),
			auth_uri: "https://accounts.google.com/o/oauth2/auth".to_string(),
			token_uri: "https://oauth2.googleapis.com/token".to_string(),
			..Default::default()
		};

		let auth = InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
			.persist_tokens_to_disk(Path::new(&oauth.token_path))
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
		let auth_wrapper = AuthWrapper(Arc::new(auth));

		Ok(Gmail::new(client, auth_wrapper))
	}

	async fn run_oauth(&self, oauth: &OAuthAuth) -> Result<()> {
		let hub = self.create_gmail_hub(oauth).await?;
		log!("Successfully authenticated with Gmail API");

		let messages = self.fetch_unread_messages_oauth(&hub).await?;
		info!("Found {} unread messages", messages.len());

		for message in messages {
			if let Err(e) = self.process_message_oauth(&hub, &message).await {
				let message_id = message.id.as_deref().unwrap_or("unknown");
				let from = self.extract_header(&message, "From").unwrap_or_else(|| "Unknown".to_string());
				error!("Failed to process message {message_id} from {from}: {e:#}");
			}
		}

		Ok(())
	}

	async fn fetch_unread_messages_oauth(&self, hub: &Gmail<HttpsConnector<HttpConnector>>) -> Result<Vec<Message>> {
		use futures::stream::{self, StreamExt};

		let mut all_messages = Vec::new();
		let mut page_token: Option<String> = Some(String::new()); // empty string = first page

		while let Some(token) = page_token.take() {
			let mut request = hub.users().messages_list(&self.config.email).q("is:unread").max_results(500);
			if !token.is_empty() {
				request = request.page_token(&token);
			}

			let result = request.doit().await.map_err(|e| color_eyre::eyre::eyre!("Failed to fetch messages: {e:#?}"))?;

			if let Some(msg_list) = result.1.messages {
				let ids: Vec<String> = msg_list.into_iter().filter_map(|m| m.id).collect();
				let email = self.config.email.clone();
				let messages: Vec<_> = stream::iter(ids)
					.map(|id| {
						let email = email.clone();
						async move { hub.users().messages_get(&email, &id).format("full").doit().await.ok() }
					})
					.buffer_unordered(50)
					.collect()
					.await;

				for msg_result in messages.into_iter().flatten() {
					all_messages.push(msg_result.1);
				}
			}

			page_token = result.1.next_page_token;
		}

		Ok(all_messages)
	}

	async fn process_message_oauth(&self, hub: &Gmail<HttpsConnector<HttpConnector>>, message: &Message) -> Result<()> {
		let message_id = message.id.as_ref().wrap_err("Message has no ID")?;

		let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());
		let subject = self.extract_header(message, "Subject").unwrap_or_else(|| "No Subject".to_string());
		let date = self.extract_header(message, "Date").unwrap_or_else(|| "Unknown".to_string());
		let reply_to = self.extract_header(message, "Reply-To");
		let list_unsubscribe = self.extract_header(message, "List-Unsubscribe");
		let body_preview = message.snippet.as_deref().unwrap_or("").to_string();

		let extra_headers = if let Some(payload) = &message.payload {
			if let Some(headers) = &payload.headers {
				headers
					.iter()
					.filter(|h| {
						matches!(
							h.name.as_deref(),
							Some("X-Mailer") | Some("User-Agent") | Some("X-Auto-Response-Suppress") | Some("Auto-Submitted") | Some("Precedence")
						)
					})
					.filter_map(|h| Some(format!("{}: {}", h.name.as_deref()?, h.value.as_deref()?)))
					.collect::<Vec<_>>()
					.join("\n")
			} else {
				String::new()
			}
		} else {
			String::new()
		};

		let email_msg = EmailMessage {
			id: message_id.clone(),
			from,
			subject,
			date,
			body_preview,
			reply_to,
			list_unsubscribe,
			extra_headers,
		};

		// Check if already processed
		if self.db.is_email_processed(&email_msg.id).await? {
			debug!("Message {} already processed, skipping", email_msg.id);
			return Ok(());
		}

		// Check ignore patterns
		if self.should_ignore(&email_msg.from) {
			debug!("Ignoring email from: {} (matches ignore pattern)", email_msg.from);
			self.mark_as_read_oauth(hub, message_id).await?;
			self.db.mark_email_processed(&email_msg.id, &email_msg.from, &email_msg.subject, false).await?;
			return Ok(());
		}

		// Determine if alert-worthy: either matches important pattern OR is from human (LLM check)
		let alert_worthy = if self.matches_important_pattern(&email_msg) {
			log!("Email matches important pattern, marking as alert-worthy: {}", email_msg.from);
			true
		} else {
			self.eval_is_human(&email_msg).await?
		};

		if alert_worthy {
			self.forward_to_telegram(&email_msg.from, &email_msg.subject, &email_msg.body_preview).await?;
			log!("Forwarded alert-worthy email from: {}", email_msg.from);
			self.db.mark_email_processed(&email_msg.id, &email_msg.from, &email_msg.subject, alert_worthy).await?;
		} else {
			self.mark_as_read_oauth(hub, message_id).await?;
			elog!("Marked non-alert email as read: {}", email_msg.from);
			self.db.mark_email_processed(&email_msg.id, &email_msg.from, &email_msg.subject, alert_worthy).await?;
		}

		Ok(())
	}

	async fn mark_as_read_oauth(&self, hub: &Gmail<HttpsConnector<HttpConnector>>, message_id: &str) -> Result<()> {
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

	async fn mark_all_as_read_oauth(&self, oauth: &OAuthAuth) -> Result<()> {
		let hub = self.create_gmail_hub(oauth).await?;

		println!("Fetching next batch of unread messages...");
		let mut messages = self.fetch_unread_messages_oauth(&hub).await?;

		while !messages.is_empty() {
			let count = messages.len();
			println!("Marking {count} unread messages as read...");

			for (i, message) in messages.iter().enumerate() {
				let message_id = message.id.clone().unwrap_or_default();
				let from = self.extract_header(message, "From").unwrap_or_else(|| "Unknown".to_string());

				if !message_id.is_empty() {
					self.mark_as_read_oauth(&hub, &message_id).await?;
				}
				println!("[{}/{}] Marked as read: {}", i + 1, count, from);
			}

			if count < 100 {
				break;
			}

			println!("Fetching next batch of unread messages...");
			messages = self.fetch_unread_messages_oauth(&hub).await?;
		}

		println!("\nAll done!");
		Ok(())
	}

	fn extract_header(&self, message: &Message, header_name: &str) -> Option<String> {
		message.payload.as_ref()?.headers.as_ref()?.iter().find(|h| h.name.as_deref() == Some(header_name))?.value.clone()
	}

	// ==================== Common Logic ====================

	fn process_email_common<F>(&self, email: &EmailMessage, mark_as_read: F) -> Result<()>
	where
		F: FnOnce(&str) -> Result<()>, {
		let rt = tokio::runtime::Handle::current();

		// Check if already processed
		if rt.block_on(async { self.db.is_email_processed(&email.id).await })? {
			debug!("Message {} already processed, skipping", email.id);
			return Ok(());
		}

		// Check ignore patterns
		if self.should_ignore(&email.from) {
			debug!("Ignoring email from: {} (matches ignore pattern)", email.from);
			mark_as_read(&email.id)?;
			rt.block_on(async { self.db.mark_email_processed(&email.id, &email.from, &email.subject, false).await })?;
			return Ok(());
		}

		// Determine if alert-worthy: either matches important pattern OR is from human (LLM check)
		let alert_worthy = if self.matches_important_pattern(email) {
			log!("Email matches important pattern, marking as alert-worthy: {}", email.from);
			true
		} else {
			rt.block_on(async { self.eval_is_human(email).await })?
		};

		if alert_worthy {
			rt.block_on(async { self.forward_to_telegram(&email.from, &email.subject, &email.body_preview).await })?;
			log!("Forwarded alert-worthy email from: {}", email.from);
			rt.block_on(async { self.db.mark_email_processed(&email.id, &email.from, &email.subject, alert_worthy).await })?;
		} else {
			mark_as_read(&email.id)?;
			elog!("Marked non-alert email as read: {}", email.from);
			rt.block_on(async { self.db.mark_email_processed(&email.id, &email.from, &email.subject, alert_worthy).await })?;
		}

		Ok(())
	}

	fn should_ignore(&self, from: &str) -> bool {
		self.ignore_regexes.iter().any(|regex| regex.is_match(from))
	}

	/// Check if email matches any of the configured important patterns.
	/// Returns true if email should be marked as alert-worthy without LLM evaluation.
	fn matches_important_pattern(&self, email: &EmailMessage) -> bool {
		let patterns = &self.config.important_if_contains;

		for pattern in &patterns.any {
			if email.subject.contains(pattern) || email.body_preview.contains(pattern) || email.from.contains(pattern) {
				debug!("Email matches important pattern '{pattern}' (any field)");
				return true;
			}
		}

		for pattern in &patterns.subject {
			if email.subject.contains(pattern) {
				debug!("Email matches important pattern '{pattern}' (subject)");
				return true;
			}
		}

		for pattern in &patterns.body {
			if email.body_preview.contains(pattern) {
				debug!("Email matches important pattern '{pattern}' (body)");
				return true;
			}
		}

		for pattern in &patterns.address {
			if email.from.contains(pattern) {
				debug!("Email matches important pattern '{pattern}' (address)");
				return true;
			}
		}

		false
	}

	#[instrument(skip(self, body))]
	async fn forward_to_telegram(&self, from: &str, subject: &str, body: &str) -> Result<()> {
		let text = format!("📧 New Email\n\nFrom: {from}\nSubject: {subject}\n\n{body}");
		self.notifier.send_message_to_alerts(&text).await?;
		info!("Forwarded email from {from} to Telegram");
		Ok(())
	}

	async fn eval_is_human(&self, message: &EmailMessage) -> Result<bool> {
		if let Some(ref token) = self.config.claude_token {
			// SAFETY: This is only called from the single-threaded main task, and is setting an env var
			// that is only read by the ask_llm crate during the subsequent API call in this same function.
			unsafe {
				std::env::set_var("CLAUDE_TOKEN", token);
			}
		}

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
			message.from,
			message.subject,
			message.date,
			message.reply_to.as_deref().unwrap_or("N/A"),
			message.list_unsubscribe.as_deref().unwrap_or("N/A"),
			if message.extra_headers.is_empty() { "None" } else { &message.extra_headers },
			message.body_preview
		);

		debug!("Calling LLM for email from: {}", message.from);
		let response = match ask_llm::Client::default().model(ask_llm::Model::Fast).ask(&prompt).await {
			Ok(r) => r,
			Err(e) => {
				error!("LLM call failed for email from {}: {e:#}", message.from);
				return Err(e).context("Failed to call LLM for email evaluation");
			}
		};

		let is_human = response.text.trim().to_lowercase().starts_with("yes");

		debug!(
			"LLM evaluation for email from {}: {} (cost: {:.4} cents)",
			message.from,
			if is_human { "HUMAN" } else { "AUTOMATED" },
			response.cost_cents
		);

		Ok(is_human)
	}
}

/// Parse a raw RFC822 message and return a decoded, human-readable body preview.
/// Prefers the text/plain part; falls back to tag-stripped HTML. Truncated to 500 chars.
fn decode_body_preview(raw: &[u8]) -> String {
	let Some(parsed) = mail_parser::MessageParser::default().parse(raw) else {
		return String::new();
	};
	let text = parsed
		.body_text(0)
		.map(|t| t.into_owned())
		.or_else(|| parsed.body_html(0).map(|h| strip_html_tags(&h)))
		.unwrap_or_default();
	text.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(500).collect()
}

fn strip_html_tags(html: &str) -> String {
	let mut out = String::with_capacity(html.len());
	let mut in_tag = false;
	for c in html.chars() {
		match c {
			'<' => in_tag = true,
			'>' => in_tag = false,
			_ if !in_tag => out.push(c),
			_ => {}
		}
	}
	out
}

fn __default_email_token_path() -> String {
	let xdg_dirs = xdg::BaseDirectories::with_prefix("social_networks");
	xdg_dirs.place_state_file("gmail_tokens.json").unwrap().display().to_string()
}

impl AdapterClient for EmailMonitor {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		println!("Email: Listening...");
		info!("Monitoring email: {}", self.config.email);

		let mut was_error = false;
		loop {
			match self.run().await {
				Ok(()) => {
					if was_error {
						info!("Email monitor reconnected successfully");
						was_error = false;
					}
					time::sleep(Duration::from_secs(60)).await;
				}
				Err(e) => {
					if let Some(detail) = classify_email_auth_error(&e) {
						return Err(AdapterError::Auth { surface: SURFACE, detail });
					}
					error!("Email monitor error: {e:#}");
					error!("Retrying in 5 minutes...");
					time::sleep(Duration::from_secs(5 * 60)).await;
					was_error = true;
				}
			}
		}
	}
}

/// Look at the error chain (string-matched) to decide whether this is an auth-class error.
/// Returns `Some(detail)` for auth errors so the caller can promote to `AdapterError::Auth`.
fn classify_email_auth_error(e: &color_eyre::eyre::Report) -> Option<String> {
	let s = format!("{e:#}");
	let lc = s.to_lowercase();
	let is_auth = lc.contains("imap login failed")
		|| lc.contains("authenticationfailed")
		|| lc.contains("invalid_grant")
		|| lc.contains("invalid_credentials")
		|| lc.contains("token expired")
		|| lc.contains("unauthorized")
		|| lc.contains(" 401")
		|| lc.contains(" 403");
	if is_auth { Some(s) } else { None }
}

#[derive(Clone)]
struct AuthWrapper(Arc<yup_oauth2::authenticator::Authenticator<HttpsConnector<HttpConnector>>>);

impl google_gmail1::common::GetToken for AuthWrapper {
	fn get_token<'a>(&'a self, _scopes: &'a [&str]) -> Pin<Box<dyn Future<Output = Result<Option<String>, Box<dyn std::error::Error + Send + Sync>>> + Send + 'a>> {
		let auth = self.0.clone();
		Box::pin(async move {
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

// Custom flow delegate to print a nice URL with tmux link support
struct CustomFlowDelegate;

impl InstalledFlowDelegate for CustomFlowDelegate {
	fn present_user_url<'a>(&'a self, url: &'a str, need_code: bool) -> Pin<Box<dyn Future<Output = std::result::Result<String, String>> + Send + 'a>> {
		Box::pin(async move {
			if need_code {
				println!("\n\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\\n");
				use std::io::{self, BufRead};
				let mut code = String::new();
				io::stdin().lock().read_line(&mut code).map_err(|e| e.to_string())?;
				Ok(code.trim().to_string())
			} else {
				println!("\n\x1b]8;;{url}\x1b\\{url}\x1b]8;;\x1b\\\n");
				Ok(String::new())
			}
		})
	}
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

/// Parsed email message (used for both IMAP and OAuth paths)
#[derive(Clone, Debug)]
struct EmailMessage {
	id: String,
	from: String,
	subject: String,
	date: String,
	body_preview: String,
	reply_to: Option<String>,
	list_unsubscribe: Option<String>,
	extra_headers: String,
}
