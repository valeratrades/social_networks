//! Poll/info channel forwarding. It rides the `dms` telegram connection rather than holding its own:
//! telegram answers a second concurrent connection on one auth key with `AUTH_KEY_DUPLICATED`.

use color_eyre::eyre::{Result, bail, eyre};
use grammers_client::{Client, update::Update};
use grammers_session::types::{PeerId, PeerRef};
use jiff::{SignedDuration, Timestamp};
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info};

use crate::telegram_dms::{TelegramConfig, TelegramDestination};

#[derive(Debug, Default, Deserialize, Serialize)]
struct StatusDrop {
	status: String,
}

pub(crate) struct ChannelWatch {
	poll: Vec<PeerId>,
	info: Vec<PeerId>,
	output: PeerRef,
	status: String,
	last_status_update: Timestamp,
}

impl ChannelWatch {
	/// Every call here is an RPC: the caller drives the runner alongside it.
	pub(crate) async fn resolve(client: &Client, config: &TelegramConfig) -> Result<Self> {
		let status_file = xdg::BaseDirectories::with_prefix("social_networks").place_state_file("telegram_status.json")?;
		let status = match status_file.exists() {
			true => serde_json::from_str::<StatusDrop>(&std::fs::read_to_string(&status_file)?)?.status,
			false => String::new(),
		};

		let resolve = async |channels: &[String]| -> Result<Vec<PeerId>> {
			let mut ids = Vec::with_capacity(channels.len());
			for channel in channels {
				match client.resolve_username(channel.trim_start_matches("https://t.me/")).await? {
					Some(peer) => ids.push(peer.id()),
					None => error!("Could not resolve channel: {channel}"),
				}
			}
			Ok(ids)
		};
		let poll = resolve(&config.poll_channels).await?;
		let info = resolve(&config.info_channels).await?;

		let output_username = match &config.channel_output {
			TelegramDestination::Channel(tg::TopLevelId::AtName(name)) | TelegramDestination::Group(tg::TopLevelId::AtName(name)) => name.trim_start_matches('@'),
			_ => bail!("channel_output must be a username for grammers client forwarding"),
		};
		let output = client
			.resolve_username(output_username)
			.await?
			.ok_or_else(|| eyre!("Could not resolve output channel: {output_username}"))?
			.to_ref()
			.await
			.map_err(|e| eyre!(e))?
			.ok_or_else(|| eyre!("Output channel peer has no access hash: {output_username}"))?;
		info!("Channel watch: {} poll, {} info channels -> {output_username}", poll.len(), info.len());

		Ok(Self {
			poll,
			info,
			output,
			status,
			last_status_update: Timestamp::default(),
		})
	}

	/// Forwarding and the profile status are RPCs: the caller drives the runner alongside this too.
	pub(crate) async fn handle(&mut self, client: &Client, update: &Update) {
		if let Update::NewMessage(message) = update
			&& !message.outgoing()
		{
			// `peer()` only sees the update batch's own users/chats vector, which the short-update
			// forms omit entirely. `peer_id()` reads the raw message.
			let peer_id = message.peer_id();
			if self.poll.contains(&peer_id) {
				if let Err(e) = handle_poll_message(client, message, self.output).await {
					error!("Error handling poll message: {e}");
				}
			} else if self.info.contains(&peer_id)
				&& let Err(e) = handle_info_message(client, message, self.output).await
			{
				error!("Error handling info message: {e}");
			}
		}

		let now = Timestamp::now();
		if !self.status.is_empty() && now.duration_since(self.last_status_update) > SignedDuration::from_secs(4 * 60) {
			match update_profile(client, &self.status).await {
				Ok(()) => debug!("Profile status updated"),
				Err(e) => error!("Error updating profile: {e}"),
			}
			self.last_status_update = now;
		}
	}
}

async fn handle_poll_message(client: &Client, message: &grammers_client::update::Message, watch_chat: PeerRef) -> Result<()> {
	if message.media().is_some() {
		let source_ref = match message.peer_ref().await.map_err(|e| color_eyre::eyre::eyre!(e))? {
			Some(r) => r,
			None => {
				error!("Cannot forward poll message - unresolved peer");
				return Ok(());
			}
		};
		let source_name = message.peer().and_then(|p| p.name()).unwrap_or("unknown").to_string();
		client.forward_messages(watch_chat, &[message.id()], source_ref).await?;
		info!("Forwarded poll/media message from {source_name}");
	}
	Ok(())
}

async fn handle_info_message(client: &Client, message: &grammers_client::update::Message, watch_chat: PeerRef) -> Result<()> {
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
		let source_ref = match message.peer_ref().await.map_err(|e| color_eyre::eyre::eyre!(e))? {
			Some(r) => r,
			None => {
				error!("Cannot forward info message - unresolved peer");
				return Ok(());
			}
		};
		let source_name = message.peer().and_then(|p| p.name()).unwrap_or("unknown").to_string();
		client.forward_messages(watch_chat, &[message.id()], source_ref).await?;
		info!("Forwarded info message from {source_name}");
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
