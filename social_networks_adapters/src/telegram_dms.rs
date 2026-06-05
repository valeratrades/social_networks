use std::convert::Infallible;

use color_eyre::eyre::Result;
use futures::future::{Either, select};
use grammers_client::update::Update;
use grammers_tl_types as tl;
use social_networks_utils::telegram_utils::{self, ConnectionConfig, TelegramConnection};
pub use tg::TelegramDestination;
use tokio::{
	sync::mpsc::UnboundedSender,
	time::{self, Duration},
};
use tracing::{error, info};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client as AdapterClient},
	dm_event::DmEvent,
};

const SURFACE: &str = "telegram_dms";

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct TelegramConfig {
	pub bot_token: String,
	#[private_value]
	pub channel_alerts: TelegramDestination,
	#[private_value]
	pub channel_output: TelegramDestination,
	pub api_id: i32,
	pub api_hash: String,
	pub phone: String,
	pub username: String,
	#[primitives(skip)]
	pub poll_channels: Vec<String>,
	#[primitives(skip)]
	pub info_channels: Vec<String>,
}

pub struct TelegramDms {
	telegram_config: TelegramConfig,
	tx: UnboundedSender<DmEvent>,
}

impl TelegramDms {
	pub fn new(telegram_config: TelegramConfig, tx: UnboundedSender<DmEvent>) -> Self {
		Self { telegram_config, tx }
	}

	async fn connect(&self) -> Result<TelegramConnection> {
		telegram_utils::connect(ConnectionConfig {
			username: &self.telegram_config.username,
			phone: &self.telegram_config.phone,
			api_id: self.telegram_config.api_id,
			api_hash: &self.telegram_config.api_hash,
			session_suffix: "_dm",
		})
		.await
	}

	/// Run a single connect+listen cycle. Returns `Ok(())` on a recoverable disconnect,
	/// `Err(AdapterError::Auth)` on auth-class failures.
	async fn run_session(&mut self) -> Result<(), AdapterError> {
		let TelegramConnection { client, updates, mut runner } = match self.connect().await {
			Ok(c) => c,
			Err(e) => {
				if let Some(detail) = classify_telegram_auth_error(&e) {
					return Err(AdapterError::Auth { surface: SURFACE, detail });
				}
				error!("Telegram connection error: {e:#}");
				return Ok(());
			}
		};

		info!("--Telegram DM Commands-- connected and authorized");
		println!("Telegram DM Commands: Connected");

		let _ = client; // hold for the lifetime of the session
		let mut updates = Box::new(updates);

		loop {
			if telegram_utils::should_reconnect_for_stack() {
				return Ok(());
			}
			telegram_utils::log_stack("dms telegram before select");

			enum Event {
				Update(Box<Result<Update, grammers_client::InvocationError>>),
				RunnerExited,
			}

			let event = {
				let update_fut = std::pin::pin!(updates.next());
				let runner_fut = runner.as_mut();
				match select(update_fut, runner_fut).await {
					Either::Left((result, _)) => Event::Update(Box::new(result)),
					Either::Right(((), _)) => Event::RunnerExited,
				}
			};

			telegram_utils::log_stack("dms telegram after select");

			match event {
				Event::RunnerExited => {
					error!("MTProto runner exited unexpectedly, reconnecting...");
					return Ok(());
				}
				Event::Update(result) => match *result {
					Err(e) => {
						let s = format!("{e:#}");
						if classify_invocation_auth(&s) {
							return Err(AdapterError::Auth { surface: SURFACE, detail: s });
						}
						error!("Error getting next update: {s}, reconnecting...");
						return Ok(());
					}
					Ok(update) => self.handle_update(update),
				},
			}
		}
	}

	fn handle_update(&mut self, update: Update) {
		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				let Some(peer) = message.peer() else {
					error!("Skipping message with unresolved peer");
					return;
				};
				if !matches!(peer, grammers_client::peer::Peer::User(_)) {
					return;
				}
				let Some(sender) = message.sender() else { return };
				let username = sender.username().unwrap_or("unknown").to_string();
				let chat_id = peer.id().bot_api_dialog_id().to_string();
				let text = message.text().to_string();

				let _ = self.tx.send(DmEvent::Message {
					platform: "Telegram",
					sender: username,
					text,
					chat_id,
					is_dm: true,
					mentions_me: false,
					is_reply_to_me: false,
				});
			}
			// Incoming voice/video call: server sends `phoneCallRequested` to the callee.
			// Outgoing calls surface as `Waiting`, so matching `Requested` naturally filters to calls TO me.
			Update::Raw(raw) =>
				if let tl::enums::Update::PhoneCall(tl::types::UpdatePhoneCall {
					phone_call: tl::enums::PhoneCall::Requested(_),
				}) = &raw.raw
				{
					let _ = self.tx.send(DmEvent::IncomingCall { platform: "Telegram" });
				},
			_ => {}
		}
	}
}

impl AdapterClient for TelegramDms {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		loop {
			self.run_session().await?;
			error!("Telegram DMs reconnecting in 30s...");
			time::sleep(Duration::from_secs(30)).await;
		}
	}
}

/// Inspect a `color_eyre` connect-time error string for auth-class failures.
/// `SignInError` non-`PasswordRequired` variants and known unauthorized RPC
/// errors all surface as text containing one of these tokens.
pub(crate) fn classify_telegram_auth_error(e: &color_eyre::eyre::Report) -> Option<String> {
	let s = format!("{e:#}");
	if classify_invocation_auth(&s) || s.to_lowercase().contains("sign in failed") {
		Some(s)
	} else {
		None
	}
}

/// Match the canonical RPC error names that grammers stringifies for codes 401/403/303.
/// String-matching is required because grammers 0.9 doesn't expose typed variants.
pub(crate) fn classify_invocation_auth(s: &str) -> bool {
	let lc = s.to_lowercase();
	lc.contains("auth_key_unregistered")
		|| lc.contains("session_revoked")
		|| lc.contains("session_expired")
		|| lc.contains("user_deactivated")
		|| lc.contains("auth_key_invalid")
		|| lc.contains("user_deactivated_ban")
		|| lc.contains("api_id_invalid")
		|| lc.contains("phone_number_banned")
}
