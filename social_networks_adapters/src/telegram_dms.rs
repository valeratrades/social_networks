use std::{convert::Infallible, path::Path};

use color_eyre::eyre::{Report, Result, WrapErr, eyre};
use futures::future::{Either, select};
use grammers_client::{
	InvocationError,
	media::Media,
	message::Message,
	peer::{Peer, Role},
	session::types::PeerRef,
	update::Update,
};
use grammers_session::Session as _;
use grammers_tl_types as tl;
use jiff::Timestamp;
use social_networks_utils::{
	db::DbSession,
	telegram_utils::{self, ConnectionConfig, TelegramConnection},
};
pub use tg::TelegramDestination;
use tokio::{
	sync::mpsc::UnboundedSender,
	time::{self, Duration},
};
use tracing::{error, info, warn};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client as AdapterClient},
	dm_event::DmEvent,
	reach::{Attachment, Author, Direct, Item, Kind, Member, Page, Profile, Profiles, Roster, Source, Venue, VenueRef, VenueSource, Window},
};

const SURFACE: &str = "telegram_dms";

pub(crate) const RECONNECT: Duration = Duration::from_secs(30);
pub(crate) const RUNNER_EXITED: &str = "MTProto runner exited";
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
	/// JSON `{ "<session name>": {home_dc, dc_options} }`, minted by `examples/telegram_mint.rs`.
	#[serde(default)]
	#[private_value]
	pub sessions: Option<String>,
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
			session_suffix: "",
			sessions: self.telegram_config.sessions.as_deref(),
		})
		.await
	}

	/// One connect+listen cycle: `Ok(wait)` before the next one.
	async fn run_session(&mut self) -> Result<Duration, AdapterError> {
		let TelegramConnection {
			client,
			updates,
			mut runner,
			session,
		} = match self.connect().await {
			Ok(c) => c,
			Err(e) => return end_session(SURFACE, classify_report(&e), format!("connect: {e:#}")),
		};

		info!("--Telegram DM Commands-- connected and authorized");
		println!("Telegram DM Commands: Connected");

		let mut updates = Box::new(updates);

		loop {
			if telegram_utils::should_reconnect_for_stack() {
				return Ok(RECONNECT);
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
				Event::RunnerExited => return end_session(SURFACE, Class::Transient(RECONNECT), RUNNER_EXITED.into()),
				Event::Update(result) => match *result {
					Err(e) => return end_session(SURFACE, classify(&e), format!("next update: {e}")),
					// Resolving a caller costs an RPC, which the runner has to answer: awaiting the
					// handler on its own hangs the adapter on the first incoming call, forever.
					Ok(update) => {
						let handle_fut = std::pin::pin!(self.handle_update(&client, &session, update));
						if let Either::Right(((), _)) = select(handle_fut, runner.as_mut()).await {
							return end_session(SURFACE, Class::Transient(RECONNECT), RUNNER_EXITED.into());
						}
					}
				},
			}
		}
	}

	async fn handle_update(&mut self, client: &grammers_client::Client, session: &DbSession, update: Update) {
		match update {
			Update::NewMessage(message) if !message.outgoing() => {
				// `peer()`/`sender()` only see the users vector of the update batch, and a plain DM
				// arrives as `updateShortMessage`, which carries none. `peer_id()` reads the raw message.
				let peer_id = message.peer_id();
				if peer_id.kind() != grammers_session::types::PeerKind::User {
					return;
				}
				// An incoming private message has no `from_id`; its sender is the peer itself.
				let username = match message.sender() {
					Some(sender) => sender.username().unwrap_or("unknown").to_string(),
					None => resolve_username(client, session, peer_id).await,
				};

				// A call that already ended lands in the dialog as a service message. This is the
				// only path that sees calls placed while we were disconnected.
				if matches!(message.action(), Some(tl::enums::MessageAction::PhoneCall(_))) {
					let _ = self.tx.send(DmEvent::IncomingCall {
						platform: "Telegram",
						caller: username,
					});
					return;
				}

				let chat_id = peer_id.bot_api_dialog_id().expect("incoming DM peer is never self").to_string();
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
			// Call still ringing: the server sends `phoneCallRequested` to the callee.
			// Outgoing calls surface as `Waiting`, so matching `Requested` naturally filters to calls TO me.
			Update::Raw(raw) => {
				if let tl::enums::Update::PhoneCall(tl::types::UpdatePhoneCall {
					phone_call: tl::enums::PhoneCall::Requested(call),
				}) = &raw.raw
				{
					let admin_id = grammers_session::types::PeerId::user(call.admin_id).expect("`admin_id` of an incoming call is a user");
					let caller = resolve_username(client, session, admin_id).await;
					let _ = self.tx.send(DmEvent::IncomingCall { platform: "Telegram", caller });
				}
			}
			_ => {}
		}
	}
}

/// The on-demand axis over a session the caller already holds. The MTProto runner has to be polled
/// alongside every call here, so the client is borrowed rather than owned: whoever drives the runner
/// owns it.
pub struct Reach<'a> {
	pub client: &'a grammers_client::Client,
}
impl Reach<'_> {
	/// A handle is a public @username. Telegram has no other global address, which is also why a
	/// group without one cannot be a [`VenueRef`].
	async fn peer(&self, handle: &str) -> Result<PeerRef> {
		let handle = handle.trim_start_matches('@');
		let peer = self.client.resolve_username(handle).await?.ok_or_else(|| eyre!("no such telegram username: `{handle}`"))?;
		peer.to_ref().await.map_err(|e| eyre!("{e}"))?.ok_or_else(|| eyre!("`{handle}` resolved but has no usable ref"))
	}

	/// `offset_id` walks strictly older and `0` means "from the newest", so both windows are the same
	/// walk under different stops.
	async fn walk(&self, peer: PeerRef, window: &Window, kind: Kind, assets: &Path, attribute: impl Fn(&Message) -> Author) -> Result<Page> {
		let (after, offset) = match window {
			Window::Above { after, .. } => (after.as_ref().map(|a| a.parse::<i32>()).transpose().wrap_err("a telegram checkpoint is a message id")?, 0),
			Window::Below { before, .. } => (
				None,
				before
					.as_ref()
					.map(|b| b.parse::<i32>())
					.transpose()
					.wrap_err("a telegram backfill floor is a message id")?
					.unwrap_or(0),
			),
		};
		let limit = window.limit();
		let mut iter = self.client.iter_messages(peer).offset_id(offset);
		let mut raw: Vec<Message> = Vec::new();
		let mut exhausted = false;
		//LOOP: newest-first over a finite history, stopped by the checkpoint, the floor or `limit`
		loop {
			let Some(message) = iter.next().await? else {
				exhausted = true;
				break;
			};
			if after.is_some_and(|c| message.id() <= c) {
				break;
			}
			if window.reached(at(&message)?) {
				exhausted = true;
				break;
			}
			raw.push(message);
			if raw.len() >= limit {
				break;
			}
		}

		let mut items = Vec::with_capacity(raw.len());
		for message in &raw {
			items.push(Item {
				id: message.id().to_string(),
				source: Source::Telegram,
				at: at(message)?,
				kind,
				author: attribute(message),
				text: message.text().to_string(),
				attachments: self.attachments(message, assets).await?,
				// telegram DMs have no per-message URL, and a public venue's is built off its slug
				permalink: None,
			});
		}
		items.sort_by_key(|item| item.at);
		Ok(Page {
			newest: raw.iter().map(|m| m.id()).max().map(|id| id.to_string()),
			oldest: raw.iter().map(|m| m.id()).min().map(|id| id.to_string()),
			exhausted,
			items,
		})
	}

	/// One media per message: an album arrives as several messages, each carrying its own.
	async fn attachments(&self, message: &Message, assets: &Path) -> Result<Vec<Attachment>> {
		let Some(media) = message.media() else { return Ok(Vec::new()) };
		let (name, mime) = match &media {
			Media::Photo(_) => (format!("photo-{}.jpg", message.id()), "image/jpeg".to_string()),
			Media::Document(document) => (document.name().unwrap_or("attachment").to_string(), document.mime_type().unwrap_or_default().to_string()),
			// a sticker, a poll, a location: nothing with bytes worth a file, and still worth a mark
			_ => ("attachment".to_string(), String::new()),
		};

		let file = format!("telegram-{}.avif", message.id());
		if assets.join(&file).exists() {
			return Ok(vec![Attachment::Image { file }]);
		}
		if !mime.starts_with("image/") {
			return Ok(vec![Attachment::File { name }]);
		}

		let mut bytes = Vec::new();
		let mut download = self.client.iter_download(&media);
		while let Some(chunk) = download.next().await? {
			bytes.extend_from_slice(&chunk);
		}
		Ok(vec![Attachment::keep(&bytes, &mime, name, assets, file)])
	}
}

/// Updates that carry no users vector (`phoneCallRequested`, `updateShortMessage`) give only an id.
/// Resolving it to the same username the full-update path reports is what lets the consumer collapse
/// the two into one alert. Losing the name is not worth losing the event, so every failure degrades
/// to the bare id rather than dropping it.
async fn resolve_username(client: &grammers_client::Client, session: &DbSession, peer_id: grammers_session::types::PeerId) -> String {
	let bare = peer_id.bare_id_unchecked();
	let peer_ref = match session.peer_ref(peer_id).await {
		Ok(Some(r)) => r,
		Ok(None) => {
			error!("Peer {bare} is not in the peer cache");
			return bare.to_string();
		}
		Err(e) => {
			error!("Peer cache lookup failed for {bare}: {e}");
			return bare.to_string();
		}
	};
	match client.resolve_peer(peer_ref).await {
		Ok(peer) => peer.username().unwrap_or("unknown").to_string(),
		Err(e) => {
			error!("Could not resolve peer {bare}: {e}");
			bare.to_string()
		}
	}
}

impl AdapterClient for TelegramDms {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		loop {
			time::sleep(self.run_session().await?).await;
		}
	}
}

impl Profiles for Reach<'_> {
	/// Telegram publishes no per-person feed, so the window bounds nothing here.
	async fn profile(&mut self, handle: &str, _window: Window) -> Result<Profile> {
		let peer = self.peer(handle).await?;
		let tl::enums::users::UserFull::Full(full) = self.client.invoke(&tl::functions::users::GetFullUser { id: peer.into() }).await?;
		let tl::enums::UserFull::Full(user_full) = full.full_user;

		let mut profile = Profile::default();
		profile.state("telegram:about", user_full.about.as_deref());
		// a handle only ever resolves to a user here; a group of that name is somebody else's mistake
		if let Peer::User(user) = self.client.resolve_peer(peer).await? {
			profile.state("telegram:name", Some(&user.full_name()));
		}
		Ok(profile)
	}
}

impl Direct for Reach<'_> {
	async fn direct(&mut self, handle: &str, window: Window, assets: &Path) -> Result<Page> {
		let peer = self.peer(handle).await?;
		let them = handle.trim_start_matches('@').to_string();
		self.walk(peer, &window, Kind::Direct, assets, |message| match message.outgoing() {
			true => Author::Me,
			false => Author::Handle(them.clone()),
		})
		.await
	}

	async fn send(&mut self, handle: &str, text: &str) -> Result<()> {
		let peer = self.peer(handle).await?;
		self.client.send_message(peer, text).await?;
		Ok(())
	}
}

impl Venue for Reach<'_> {
	/// Only the groups and channels carrying a public @username: that is the whole of telegram's
	/// global address space, and a venue nobody can name again is not one a roster can be filed under.
	async fn venues(&mut self) -> Result<Vec<VenueRef>> {
		let mut out = Vec::new();
		let mut unnamed = 0usize;
		let mut dialogs = self.client.iter_dialogs();
		//LOOP: over a finite dialog list
		while let Some(dialog) = dialogs.next().await? {
			let (title, username) = match &dialog.peer {
				Peer::User(_) => continue,
				Peer::Group(group) => (group.title().unwrap_or_default(), group.username()),
				Peer::Channel(channel) => (channel.title(), channel.username()),
			};
			match username {
				Some(username) => out.push(VenueRef {
					platform: VenueSource::Telegram,
					slug: username.to_string(),
					display: title.to_string(),
				}),
				None => unnamed += 1,
			}
		}
		if unnamed > 0 {
			warn!("telegram: {unnamed} groups have no public @username and cannot be addressed");
		}
		Ok(out)
	}

	async fn members(&mut self, at: &VenueRef, roster: &mut impl Roster) -> Result<()> {
		let peer = self.peer(&at.slug).await?;
		let mut out = Vec::new();
		let mut participants = self.client.iter_participants(peer);
		//LOOP: over a finite roster
		while let Some(participant) = participants.next().await? {
			let user = participant.user;
			out.push(Member {
				// an account without a public @username is addressable by nothing else telegram-wide
				handle: user.username().map_or_else(|| user.id().bare_id_unchecked().to_string(), str::to_string),
				display: user.full_name(),
				joined: match &participant.role {
					Role::User(normal) => Some(Timestamp::from_second(normal.date().timestamp()).wrap_err("a telegram join date is a unix second")?),
					_ => None,
				},
				// telegram states where nobody is
				lat: None,
				lon: None,
				zone: None,
				place: None,
				bio: None,
			});
		}
		roster.check_in(&out, None).map(|_| ())
	}

	async fn posts(&mut self, at: &VenueRef, window: Window, assets: &Path) -> Result<Page> {
		let peer = self.peer(&at.slug).await?;
		let slug = at.slug.clone();
		self.walk(peer, &window, Kind::Post, assets, |message| match message.outgoing() {
			true => Author::Me,
			// a channel posts as itself, and a member without a public @username as their id
			false => Author::Handle(
				message
					.sender()
					.and_then(|s| s.username().map(str::to_string))
					.or_else(|| message.sender_id().map(|id| id.bare_id_unchecked().to_string()))
					.unwrap_or_else(|| slug.clone()),
			),
		})
		.await
	}
}

fn at(message: &Message) -> Result<Timestamp> {
	Timestamp::from_second(message.date().timestamp()).wrap_err("a telegram message date is a unix second")
}

/// What a failed MTProto call means for the session that made it.
#[derive(Debug, PartialEq)]
pub(crate) enum Class {
	Transient(Duration),
	Auth,
	Unhandled,
}

pub(crate) fn classify(e: &InvocationError) -> Class {
	use grammers_mtproto::transport;
	const AUTH: [&str; 5] = ["AUTH_KEY_*", "SESSION_*", "USER_DEACTIVATED*", "API_ID_INVALID", "PHONE_NUMBER_BANNED"];
	match e {
		InvocationError::Io(_) | InvocationError::Dropped => Class::Transient(RECONNECT),
		InvocationError::Transport(transport::Error::BadStatus { status: 404 }) => Class::Auth, // the server no longer knows our key
		InvocationError::Transport(_) => Class::Transient(RECONNECT),
		InvocationError::Rpc(rpc) if rpc.code == 420 => Class::Transient(Duration::from_secs(rpc.value.expect("420s are FLOOD_WAIT_<seconds>").into())),
		InvocationError::Rpc(rpc) if rpc.code >= 500 => Class::Transient(RECONNECT),
		InvocationError::Rpc(rpc) if rpc.code == 401 || AUTH.iter().any(|name| rpc.is(name)) => Class::Auth,
		_ => Class::Unhandled,
	}
}

/// Connect fails through `eyre`, with the MTProto error, if any, somewhere in the chain.
pub(crate) fn classify_report(e: &Report) -> Class {
	match e.chain().find_map(|c| c.downcast_ref::<InvocationError>()) {
		Some(e) => classify(e),
		None if format!("{e:#}").contains("Sign in failed") => Class::Auth,
		None => Class::Unhandled,
	}
}

/// `Ok(wait)` before reconnecting, or the error that ends the surface.
pub(crate) fn end_session(surface: &'static str, class: Class, detail: String) -> Result<Duration, AdapterError> {
	match class {
		Class::Transient(wait) => {
			warn!("{surface}: {detail}, reconnecting in {wait:?}");
			Ok(wait)
		}
		Class::Auth => Err(AdapterError::Auth { surface, detail }),
		Class::Unhandled => Err(AdapterError::Unhandled { surface, detail }),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_known_rpc_errors_are_retried() {
		let rpc = |code, name: &str| {
			InvocationError::Rpc(
				tl::types::RpcError {
					error_code: code,
					error_message: name.into(),
				}
				.into(),
			)
		};
		assert_eq!(classify(&rpc(406, "AUTH_KEY_DUPLICATED")), Class::Auth);
		assert_eq!(classify(&rpc(401, "AUTH_KEY_UNREGISTERED")), Class::Auth);
		assert_eq!(classify(&rpc(400, "CHANNEL_INVALID")), Class::Unhandled);
		assert_eq!(classify(&rpc(420, "FLOOD_WAIT_31")), Class::Transient(Duration::from_secs(31)));
		assert_eq!(classify(&rpc(500, "INTERDC_2_CALL_ERROR")), Class::Transient(RECONNECT));
		assert_eq!(classify(&InvocationError::Dropped), Class::Transient(RECONNECT));
	}
}
