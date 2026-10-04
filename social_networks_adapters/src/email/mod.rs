use std::{convert::Infallible, future::Future, io::Cursor, path::Path, pin::Pin, sync::Arc};

use clap::Args;
use color_eyre::eyre::{Context, ContextCompat, Result, eyre};
use google_gmail1::{Gmail, api::Message};
use hyper_rustls::{HttpsConnector, HttpsConnectorBuilder};
use hyper_util::client::legacy::{Client, connect::HttpConnector};
use imap::{ImapConnection, Session};
use imap_proto::NameAttribute;
use jiff::Timestamp;
use lettre::{AsyncSmtpTransport, AsyncTransport, Tokio1Executor, transport::smtp::authentication::Credentials};
use regex::Regex;
use serde::{Deserialize, Serialize};
use social_networks_utils::db::Database;
use tokio::{
	runtime::Handle,
	time::{self, Duration},
};
use tracing::{debug, error, info, instrument};
use v_utils::{log, macros::MyConfigPrimitives};
use yup_oauth2::{ApplicationSecret, InstalledFlowAuthenticator, InstalledFlowReturnMethod, authenticator_delegate::InstalledFlowDelegate};

pub use self::script::Scripts;
use self::script::{Reply, Script, Step};
use crate::{
	breaker::CircuitBreakers,
	client::{AdapterError, Client as AdapterClient},
	llm::LlmConfig,
	telegram_dms::TelegramConfig,
	telegram_notifier::TelegramNotifier,
};

mod script;

const SURFACE: &str = "email";
type Hub = Gmail<HttpsConnector<HttpConnector>>;
type ImapSession = Session<Box<dyn ImapConnection>>;
#[derive(Args)]
pub struct EmailArgs {
	/// Mark all unread emails as read without processing
	#[arg(long)]
	pub mark_all_read: bool,
	/// Post what a script would send to the Telegram alerts channel instead of mailing it
	#[arg(long)]
	pub dry_run: bool,
}
#[derive(Clone, Debug, MyConfigPrimitives)]
#[primitives(skip_serialize)]
pub struct EmailConfig {
	/// Gmail email address to monitor
	pub email: String,
	/// Authentication method (IMAP or OAuth)
	#[primitives(skip)]
	pub auth: EmailAuth,
	#[serde(default)]
	#[primitives(skip)]
	pub rules: Rules,
	#[serde(default)]
	#[primitives(skip)]
	pub scripts: Scripts,
}

/// The definitive decision for an email, whatever heuristic produced it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
	/// notify, leave unread
	Important,
	/// no notify, leave unread
	ReadLater,
	/// no notify, mark read
	Discard,
}
impl Action {
	fn as_str(self) -> &'static str {
		match self {
			Self::Important => "important",
			Self::ReadLater => "read_later",
			Self::Discard => "discard",
		}
	}
}

/// Regex patterns, checked in field order: Important > ReadLater > Discard. Unmatched → LLM.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Rules {
	#[serde(default)]
	pub important: Match,
	#[serde(default)]
	pub read_later: Match,
	#[serde(default)]
	pub discard: Match,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct Match {
	#[serde(default)]
	pub subject: Vec<String>,
	#[serde(default)]
	pub body: Vec<String>,
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
	llm_config: LlmConfig,
	notifier: TelegramNotifier,
	db: Database,
	rules: CompiledRules,
	breakers: CircuitBreakers,
	dry_run: bool,
}
impl EmailMonitor {
	fn try_new(config: EmailConfig, llm_config: LlmConfig, notifier: TelegramNotifier, db: Database, breakers: CircuitBreakers, dry_run: bool) -> Result<Self> {
		let rules = CompiledRules::try_new(&config.rules)?;
		Ok(Self {
			config,
			llm_config,
			notifier,
			db,
			rules,
			breakers,
			dry_run,
		})
	}

	pub async fn try_from_configs(email_config: EmailConfig, llm_config: LlmConfig, telegram_config: TelegramConfig, breakers: CircuitBreakers, dry_run: bool) -> Result<Self> {
		// Install default crypto provider for rustls (needed for OAuth)
		let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
		let notifier = TelegramNotifier::new(telegram_config);
		let db = Database::try_new().await.context("Failed to open database")?;
		Self::try_new(email_config, llm_config, notifier, db, breakers, dry_run)
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

	fn connect_imap(&self) -> Result<ImapSession> {
		let pass = match &self.config.auth {
			EmailAuth::Imap(imap_auth) => &imap_auth.pass,
			EmailAuth::Oauth(_) => unreachable!(),
		};

		let client = imap::ClientBuilder::new("imap.gmail.com", 993).connect().context("Failed to connect to Gmail IMAP")?;

		let session = client.login(&self.config.email, pass).map_err(|e| eyre!("IMAP login failed: {:?}", e.0))?;

		Ok(session)
	}

	/// Every folder at once, sent mail included, so a thread can be read whole.
	fn connect_all_mail(&self) -> Result<ImapSession> {
		let mut session = self.connect_imap()?;
		let names = session.list(None, Some("*")).context("Failed to list mailboxes")?;
		let all = names
			.iter()
			.find(|n| n.attributes().contains(&NameAttribute::All))
			.context("no mailbox carries SPECIAL-USE \\All; tick \"Show in IMAP\" for All Mail in Gmail's label settings")?
			.name()
			.to_owned();
		session.examine(&all).with_context(|| format!("Failed to examine {all}"))?;
		Ok(session)
	}

	async fn run_imap(&self) -> Result<()> {
		let this = self.clone();

		tokio::task::spawn_blocking(move || {
			let rt = Handle::current();
			let mut session = this.connect_imap()?;
			session.select("INBOX").context("Failed to select INBOX")?;

			let uids = session.uid_search("UNSEEN").context("Failed to search for unread messages")?;
			let mut new = Vec::new();
			for uid in uids.iter() {
				let id = format!("{}/imap-{uid}", this.config.email);
				if !rt.block_on(this.db.is_email_processed(&id))? {
					new.push((*uid, id));
				}
			}
			info!("Found {} unread messages, {} new", uids.len(), new.len());

			let mut all_mail = None;
			for (uid, id) in new {
				if all_mail.is_none() {
					all_mail = Some(this.connect_all_mail()?);
				}
				let all_mail = all_mail.as_mut().expect("connected above");
				if let Err(e) = this.process_message_imap(&rt, &mut session, all_mail, uid, id) {
					if classify_email_auth_error(&e).is_some() {
						return Err(e);
					}
					error!("Failed to process message {uid}: {e:#}");
				}
			}

			// the run is over either way; a failed LOGOUT leaves nothing behind
			session.logout().ok();
			if let Some(mut all_mail) = all_mail {
				all_mail.logout().ok();
			}
			Ok(())
		})
		.await?
	}

	fn process_message_imap(&self, rt: &Handle, inbox: &mut ImapSession, all_mail: &mut ImapSession, uid: u32, id: String) -> Result<()> {
		let latest = EmailMessage::parse(id, &fetch_raw(inbox, uid)?)?;
		let thread = self.thread_imap(all_mail, latest)?;
		let latest = thread.last().expect("pushed last");

		let verdict = rt.block_on(self.decide(&thread, latest))?;
		if let Some(reply) = &verdict.send {
			rt.block_on(self.send_smtp(reply))?;
		}
		if verdict.mark_read {
			self.mark_as_read_imap(inbox, uid)?;
		}
		rt.block_on(self.db.mark_email_processed(&latest.id, &latest.from, &latest.subject, verdict.action))
	}

	/// Oldest first, in the order `References` lists them. A referenced message this account never held
	/// (deleted, or exchanged between others) is left out.
	fn thread_imap(&self, all_mail: &mut ImapSession, latest: EmailMessage) -> Result<Vec<EmailMessage>> {
		let mut ids = latest.references.clone();
		ids.extend(latest.in_reply_to.iter().filter(|id| !latest.references.contains(id)).cloned());

		let mut thread = Vec::with_capacity(ids.len() + 1);
		for id in ids {
			let uids = all_mail
				.uid_search(format!("HEADER Message-ID \"<{id}>\""))
				.with_context(|| format!("Failed to search for <{id}>"))?;
			// a message delivered twice carries one id
			let Some(uid) = uids.into_iter().min() else { continue };
			thread.push(EmailMessage::parse(format!("{}/imap-all-{uid}", self.config.email), &fetch_raw(all_mail, uid)?)?);
		}
		thread.push(latest);
		Ok(thread)
	}

	async fn send_smtp(&self, reply: &Reply) -> Result<()> {
		let EmailAuth::Imap(ImapAuth { pass }) = &self.config.auth else {
			unreachable!("SMTP is how IMAP accounts send")
		};
		self.breakers.admit(&self.db, &format!("email:{}", reply.to)).await?;
		let transport = AsyncSmtpTransport::<Tokio1Executor>::relay("smtp.gmail.com")?
			.credentials(Credentials::new(self.config.email.clone(), pass.clone()))
			.build();
		transport.send(lettre::Message::try_from(reply)?).await.map_err(|e| match e.status().map(u16::from) {
			Some(535) => eyre!("SMTP authentication failed: {e}"),
			_ => eyre!("SMTP send to {} failed: {e}", reply.to),
		})?;
		Ok(())
	}

	fn mark_as_read_imap(&self, session: &mut ImapSession, uid: u32) -> Result<()> {
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
				let from = EmailMessage::parse(format!("{}/imap-{uid}", this.config.email), &fetch_raw(&mut session, *uid)?)?.from;
				this.mark_as_read_imap(&mut session, *uid)?;
				println!("[{}/{}] Marked as read: {}", i + 1, count, from);
			}

			println!("\nAll done! Marked {count} messages as read.");
			// the run is over either way; a failed LOGOUT leaves nothing behind
			session.logout().ok();
			Ok(())
		})
		.await?
	}

	// ==================== OAuth/Gmail API Implementation ====================

	async fn create_gmail_hub(&self, oauth: &OAuthAuth) -> Result<Hub> {
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

		let unread = self.list_unread_oauth(&hub).await?;
		let mut new = Vec::new();
		for (gmail_id, thread_id) in &unread {
			if !self.db.is_email_processed(&format!("{}/{gmail_id}", self.config.email)).await? {
				new.push((gmail_id, thread_id));
			}
		}
		info!("Found {} unread messages, {} new", unread.len(), new.len());

		for (gmail_id, thread_id) in new {
			if let Err(e) = self.process_message_oauth(&hub, gmail_id, thread_id).await {
				if classify_email_auth_error(&e).is_some() {
					return Err(e);
				}
				error!("Failed to process message {gmail_id}: {e:#}");
			}
		}

		Ok(())
	}

	/// `(message id, thread id)` of every unread message.
	async fn list_unread_oauth(&self, hub: &Hub) -> Result<Vec<(String, String)>> {
		let mut unread = Vec::new();
		let mut page_token: Option<String> = None;
		loop {
			let mut request = hub.users().messages_list(&self.config.email).q("is:unread").max_results(500);
			if let Some(token) = &page_token {
				request = request.page_token(token);
			}
			let (_, list) = request.doit().await.map_err(|e| eyre!("Failed to fetch messages: {e:#?}"))?;
			// an empty page carries no `messages` at all
			for m in list.messages.into_iter().flatten() {
				unread.push((m.id.context("listed message has no id")?, m.thread_id.context("listed message has no thread id")?));
			}
			page_token = list.next_page_token;
			if page_token.is_none() {
				return Ok(unread);
			}
		}
	}

	async fn get_oauth(&self, hub: &Hub, gmail_id: &str) -> Result<EmailMessage> {
		let (_, message) = hub
			.users()
			.messages_get(&self.config.email, gmail_id)
			.format("raw")
			.doit()
			.await
			.with_context(|| format!("Failed to fetch message {gmail_id}"))?;
		EmailMessage::parse(format!("{}/{gmail_id}", self.config.email), &message.raw.context("`format=raw` carries `raw`")?)
	}

	/// Oldest first, as Gmail orders a thread.
	async fn thread_oauth(&self, hub: &Hub, thread_id: &str) -> Result<Vec<EmailMessage>> {
		let (_, thread) = hub
			.users()
			.threads_get(&self.config.email, thread_id)
			.format("minimal")
			.doit()
			.await
			.with_context(|| format!("Failed to fetch thread {thread_id}"))?;
		let mut messages = Vec::new();
		for m in thread.messages.context("thread has no messages")? {
			messages.push(self.get_oauth(hub, &m.id.context("thread message has no id")?).await?);
		}
		Ok(messages)
	}

	async fn process_message_oauth(&self, hub: &Hub, gmail_id: &str, thread_id: &str) -> Result<()> {
		let id = format!("{}/{gmail_id}", self.config.email);
		let thread = self.thread_oauth(hub, thread_id).await?;
		let email = thread.iter().find(|m| m.id == id).context("thread does not hold the message listed under it")?;

		let verdict = self.decide(&thread, email).await?;
		if let Some(reply) = &verdict.send {
			self.send_oauth(hub, thread_id, reply).await?;
		}
		if verdict.mark_read {
			self.mark_as_read_oauth(hub, gmail_id).await?;
		}
		self.db.mark_email_processed(&email.id, &email.from, &email.subject, verdict.action).await
	}

	async fn send_oauth(&self, hub: &Hub, thread_id: &str, reply: &Reply) -> Result<()> {
		self.breakers.admit(&self.db, &format!("email:{}", reply.to)).await?;
		let raw = lettre::Message::try_from(reply)?.formatted();
		let request = Message {
			thread_id: Some(thread_id.to_owned()),
			..Default::default()
		};
		hub.users()
			.messages_send(request, &self.config.email)
			.upload(Cursor::new(raw), "message/rfc822".parse().expect("static mime"))
			.await
			.with_context(|| format!("Failed to send reply to {}", reply.to))?;
		Ok(())
	}

	async fn mark_as_read_oauth(&self, hub: &Hub, message_id: &str) -> Result<()> {
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

		let unread = self.list_unread_oauth(&hub).await?;
		let count = unread.len();
		if count == 0 {
			println!("No unread messages found.");
			return Ok(());
		}

		println!("Marking {count} unread messages as read...");
		for (i, (gmail_id, _)) in unread.iter().enumerate() {
			let from = self.get_oauth(&hub, gmail_id).await?.from;
			self.mark_as_read_oauth(&hub, gmail_id).await?;
			println!("[{}/{}] Marked as read: {}", i + 1, count, from);
		}

		println!("\nAll done! Marked {count} messages as read.");
		Ok(())
	}

	// ==================== Common Logic ====================

	/// `email` is the unread message that brought the thread up.
	async fn decide(&self, thread: &[EmailMessage], email: &EmailMessage) -> Result<Verdict> {
		if let Some(script) = self.config.scripts.find(thread)? {
			return self.hold(script, thread, email).await;
		}
		let action = match self.rules.decide(email) {
			Some(a) => {
				log!("Email from {} matched rule: {a:?}", email.from);
				a
			}
			None => self.llm_classify(thread, email).await?,
		};
		if action == Action::Important {
			self.forward_to_telegram(email).await?;
		}
		Ok(Verdict {
			action: action.as_str(),
			send: None,
			mark_read: action == Action::Discard,
		})
	}

	/// Unread until a human reads it: a reached goal and a dry-run draft are both theirs to act on.
	async fn hold(&self, script: &Script, thread: &[EmailMessage], email: &EmailMessage) -> Result<Verdict> {
		let response = ask_llm::Client::new((&self.llm_config).into())
			.model(ask_llm::Model::Slow)
			.ask(&script.prompt(&self.config.email, thread))
			.await
			.with_context(|| format!("script `{}` on `{}`", script.name(), email.subject))?;
		debug!("script `{}` on `{}` (cost: {:.4} cents)", script.name(), email.subject, response.cost_cents);

		let verdict = Verdict {
			action: "script",
			send: None,
			mark_read: false,
		};
		match response.text.parse::<Step>()? {
			Step::Achieved => {
				let text = format!(
					"🎯 goal reached — {} → {}\n\nFrom: {}\nSubject: {}\n\n{}",
					script.name(),
					self.config.email,
					email.from,
					email.subject,
					email.body_preview()
				);
				self.notifier.send_message_to_alerts(&text).await?;
				Ok(verdict)
			}
			Step::Reply(body) => {
				let reply = Reply::try_new(&self.config.email, thread, body)?;
				if !self.dry_run {
					return Ok(Verdict {
						send: Some(reply),
						mark_read: true,
						..verdict
					});
				}
				lettre::Message::try_from(&reply)?;
				let text = format!("✍️ draft — {} → {}\n\nSubject: {}\n\n{}", script.name(), reply.to, reply.subject, reply.body);
				self.notifier.send_message_to_alerts(&text).await?;
				Ok(verdict)
			}
		}
	}

	#[instrument(skip(self, email))]
	async fn forward_to_telegram(&self, email: &EmailMessage) -> Result<()> {
		let text = format!(
			"📧 New Email → {}\n\nFrom: {}\nSubject: {}\n\n{}",
			self.config.email,
			email.from,
			email.subject,
			email.body_preview()
		);
		self.notifier.send_message_to_alerts(&text).await?;
		info!("Forwarded email from {} to Telegram", email.from);
		Ok(())
	}

	async fn llm_classify(&self, thread: &[EmailMessage], message: &EmailMessage) -> Result<Action> {
		let prompt = format!(
			r#"Analyze the latest email of this thread and determine if it's from a human or an automated system.

From: {}
Subject: {}
Reply-To: {}
List-Unsubscribe: {}

Additional Headers:
{}

Thread, oldest first:

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
			message.reply_to.as_deref().unwrap_or("N/A"),
			message.list_unsubscribe.as_deref().unwrap_or("N/A"),
			if message.extra_headers.is_empty() { "None" } else { &message.extra_headers },
			thread.iter().map(ToString::to_string).collect::<Vec<_>>().join("\n\n---\n\n"),
		);

		debug!("Calling LLM for email from: {}", message.from);
		let response = ask_llm::Client::new((&self.llm_config).into())
			.model(ask_llm::Model::Medium)
			.ask(&prompt)
			.await
			.with_context(|| format!("Failed to classify email from {}", message.from))?;

		let action = if response.text.trim().to_lowercase().starts_with("yes") {
			Action::Important
		} else {
			Action::Discard
		};

		debug!("LLM evaluation for email from {}: {action:?} (cost: {:.4} cents)", message.from, response.cost_cents);

		Ok(action)
	}
}

/// What a decided message still owes the backend it came from.
struct Verdict {
	action: &'static str,
	send: Option<Reply>,
	mark_read: bool,
}

fn fetch_raw(session: &mut ImapSession, uid: u32) -> Result<Vec<u8>> {
	let fetches = session.uid_fetch(uid.to_string(), "BODY.PEEK[]").context("Failed to fetch message")?;
	Ok(fetches.iter().next().context("Message not found")?.body().context("fetch carries no body")?.to_vec())
}

#[derive(Clone, Debug)]
struct CompiledMatch {
	subject: Vec<Regex>,
	body: Vec<Regex>,
	address: Vec<Regex>,
}
impl CompiledMatch {
	fn try_new(m: &Match) -> Result<Self> {
		let compile = |patterns: &Vec<String>| patterns.iter().map(|p| Regex::new(p).context(format!("Invalid pattern: {p}"))).collect::<Result<Vec<_>>>();
		Ok(Self {
			subject: compile(&m.subject)?,
			body: compile(&m.body)?,
			address: compile(&m.address)?,
		})
	}

	fn matches(&self, email: &EmailMessage) -> bool {
		self.subject.iter().any(|r| r.is_match(&email.subject)) || self.body.iter().any(|r| r.is_match(&email.body)) || self.address.iter().any(|r| r.is_match(&email.from))
	}
}

#[derive(Clone, Debug)]
struct CompiledRules {
	important: CompiledMatch,
	read_later: CompiledMatch,
	discard: CompiledMatch,
}
impl CompiledRules {
	fn try_new(rules: &Rules) -> Result<Self> {
		Ok(Self {
			important: CompiledMatch::try_new(&rules.important)?,
			read_later: CompiledMatch::try_new(&rules.read_later)?,
			discard: CompiledMatch::try_new(&rules.discard)?,
		})
	}

	fn decide(&self, email: &EmailMessage) -> Option<Action> {
		if self.important.matches(email) {
			Some(Action::Important)
		} else if self.read_later.matches(email) {
			Some(Action::ReadLater)
		} else if self.discard.matches(email) {
			Some(Action::Discard)
		} else {
			None
		}
	}
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

		let mut failures = 0u32;
		loop {
			match self.run().await {
				Ok(()) => {
					if failures > 0 {
						println!("Email: reconnected after {failures} failed attempts ({} min down)", failures * 5);
						info!(failures, "Email monitor reconnected");
						failures = 0;
					}
					time::sleep(Duration::from_secs(60)).await;
				}
				Err(e) => {
					if let Some(detail) = classify_email_auth_error(&e) {
						return Err(AdapterError::Auth { surface: SURFACE, detail });
					}
					failures += 1;
					error!("Email monitor error (attempt {failures}): {e:#}");
					error!("Retrying in 5 minutes...");
					time::sleep(Duration::from_secs(5 * 60)).await;
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
		|| lc.contains("smtp authentication failed")
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
			.field("dry_run", &self.dry_run)
			.finish()
	}
}

/// One message of a thread, parsed from raw RFC822 whichever backend fetched it.
#[derive(Clone, Debug)]
#[cfg_attr(test, derive(Default))]
struct EmailMessage {
	/// `<account>/<backend id>`, the dedup key
	id: String,
	/// `Name <address>`
	from: String,
	from_address: String,
	subject: String,
	date: Option<Timestamp>,
	/// Text, quoted history dropped
	body: String,
	/// Address only
	reply_to: Option<String>,
	list_unsubscribe: Option<String>,
	extra_headers: String,
	message_id: Option<String>,
	in_reply_to: Vec<String>,
	/// Oldest first
	references: Vec<String>,
}
impl EmailMessage {
	fn parse(id: String, raw: &[u8]) -> Result<Self> {
		let parsed = mail_parser::MessageParser::default().parse(raw).with_context(|| format!("{id} is not an RFC822 message"))?;
		let sender = parsed.from().and_then(|a| a.first()).with_context(|| format!("{id} has no From"))?;
		let from_address = sender.address().with_context(|| format!("{id} has a From without an address"))?.to_owned();
		let ids = |v: &mail_parser::HeaderValue| v.as_text_list().map(|ids| ids.iter().map(|id| id.to_string()).collect()).unwrap_or_default();
		Ok(Self {
			from: match sender.name() {
				Some(name) => format!("{name} <{from_address}>"),
				None => from_address.clone(),
			},
			from_address,
			subject: parsed.subject().unwrap_or_default().to_owned(),
			date: parsed.date().map(|d| Timestamp::from_second(d.to_timestamp())).transpose()?,
			// ponytail: `>`-prefixed lines are the whole of quote detection; Gmail's "On … wrote:" line stays
			body: parsed
				.body_text(0)
				.map(|t| t.lines().filter(|l| !l.starts_with('>')).collect::<Vec<_>>().join("\n"))
				.unwrap_or_default(),
			reply_to: parsed.reply_to().and_then(|a| a.first()?.address()).map(str::to_owned),
			list_unsubscribe: parsed.header_raw("List-Unsubscribe").map(|v| v.trim().to_owned()),
			extra_headers: ["X-Mailer", "User-Agent", "X-Auto-Response-Suppress", "Auto-Submitted", "Precedence"]
				.into_iter()
				.filter_map(|h| Some(format!("{h}: {}", parsed.header_raw(h)?.trim())))
				.collect::<Vec<_>>()
				.join("\n"),
			message_id: parsed.message_id().map(str::to_owned),
			in_reply_to: ids(parsed.in_reply_to()),
			references: ids(parsed.references()),
			id,
		})
	}

	fn body_preview(&self) -> String {
		self.body.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(500).collect()
	}
}
impl std::fmt::Display for EmailMessage {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "From: {}\nDate: ", self.from)?;
		match self.date {
			Some(date) => write!(f, "{date}")?,
			None => f.write_str("unknown")?,
		}
		write!(f, "\nSubject: {}\n\n{}", self.subject, self.body.trim())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rules_precedence_and_field_routing() {
		let rules = CompiledRules::try_new(&Rules {
			important: Match {
				address: vec![r"@equilibretechnologies\.com".into()],
				subject: vec!["Appointment booked".into()],
				body: vec![],
			},
			read_later: Match {
				address: vec!["Alex Hormozi".into()],
				..Default::default()
			},
			discard: Match {
				address: vec!["imperiumlabs".into()],
				..Default::default()
			},
		})
		.unwrap();

		let email = |from: &str, subject: &str| EmailMessage {
			from: from.into(),
			subject: subject.into(),
			..Default::default()
		};

		assert_eq!(rules.decide(&email("bob@equilibretechnologies.com", "hi")), Some(Action::Important));
		assert_eq!(rules.decide(&email("Alex Hormozi <a@acq.com>", "hi")), Some(Action::ReadLater));
		assert_eq!(rules.decide(&email("Alex Hormozi <a@acq.com>", "Appointment booked")), Some(Action::Important));
		assert_eq!(rules.decide(&email("noreply@imperiumlabs.io", "hi")), Some(Action::Discard));
		assert_eq!(rules.decide(&email("stranger@example.com", "hi")), None);
	}
}
