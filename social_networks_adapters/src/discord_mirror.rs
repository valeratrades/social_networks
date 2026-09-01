//! A guild we do not own, reproduced under `_` inside one we do.
//!
//! ```text
//!   source guild                                    our guild
//!   # general ──── MESSAGE_CREATE ──► render ──► POST /webhooks/{id}/{tok} ──► # _general
//!                                                  username + avatar_url            │
//!   # general ◄──── POST /channels/{src}/messages ◄──── MESSAGE_CREATE ◄────────────┘
//!                   message_reference { type: 1 }
//! ```
//!
//! The write into our guild goes over a webhook rather than the user token, because only a webhook
//! carries a per-message `username`/`avatar_url` and so can make a mirrored message wear its
//! original author. The write back into the source is a native forward, which costs reply threading
//! and pings and keeps attribution.
//!
//! `mirror_channels` and `mirror_messages` ([`Database`]) are what tie the two directions together:
//! dedup across restarts, the reply link, and the loop guard.
//!
//! Not reproduced: permission overwrites (source roles do not exist on our side), roles, emoji,
//! members, and edits or deletions — this is creates only.

use std::{collections::HashMap, convert::Infallible, sync::LazyLock};

use clap::Args;
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use futures::future::{Either, select};
use regex::Regex;
use serde_json::json;
use social_networks_utils::db::{Database, MirrorChannel};
use tokio::time;
use tracing::{error, info, warn};
use v_utils::{
	io::{ConfirmResult, confirmation},
	macros::MyConfigPrimitives,
};

use crate::{
	client::{AdapterError, Client},
	discord::{Anchor, DiscordConfig, Gateway, Rest, next_after, reconnect_delay},
	reach::PAGE,
};

const SURFACE: &str = "discord_mirror";
/// Category, text, voice, announcement, stage, forum.
const MIRRORED: [u64; 6] = [4, 0, 2, 5, 13, 15];
const CATEGORY: u64 = 4;
const FORUM: u64 = 15;
/// The channel types a thread can hang under.
const THREADED: [u64; 3] = [0, 5, 15];
const CONTENT_LIMIT: usize = 2000;
/// The upload ceiling on an unboosted guild.
const UPLOAD_LIMIT: u64 = 10 * 1024 * 1024;
#[derive(Args)]
pub struct MirrorArgs {
	/// Print the channels the topology sync would create, and create none of them.
	#[arg(long)]
	pub dry_run: bool,
}
#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct MirrorConfig {
	pub source_guild: String,
	pub target_guild: String,
}
pub struct DiscordMirror {
	config: MirrorConfig,
	token: String,
	my_username: String,
	rest: Rest,
	db: Database,
	/// src channel id -> where its messages go. Rebuilt from `mirror_channels` on every start.
	channels: HashMap<String, MirrorChannel>,
	/// dst channel id -> src channel id, for the way back
	back: HashMap<String, String>,
}
impl DiscordMirror {
	pub async fn try_new(discord: DiscordConfig, config: MirrorConfig, db: Database) -> Result<Self> {
		assert!(!config.source_guild.is_empty() && !config.target_guild.is_empty(), "[mirror] names two guilds");
		let mut this = Self {
			rest: Rest::new(discord.user_token.clone(), discord.my_username.clone()),
			token: discord.user_token,
			my_username: discord.my_username,
			config,
			db,
			channels: HashMap::new(),
			back: HashMap::new(),
		};
		for row in this.db.mirror_channels().await? {
			this.remember(row);
		}
		Ok(this)
	}

	fn remember(&mut self, row: MirrorChannel) {
		self.back.insert(row.dst_id.clone(), row.src_id.clone());
		self.channels.insert(row.src_id.clone(), row);
	}

	/// Additive and idempotent: what `mirror_channels` already holds is left alone, and nothing is
	/// ever deleted. `dry_run` walks the same source and creates nothing.
	pub async fn sync(&mut self, dry_run: bool) -> Result<()> {
		let mut source = self.rest.guild_channels(&self.config.source_guild).await?;
		// categories first: a channel's parent has to exist before the channel does
		source.sort_by_key(|c| u64::from(kind(c) != CATEGORY));

		for c in &source {
			let (id, name, ty) = (str_of(c, "id")?, str_of(c, "name")?, kind(c));
			if !MIRRORED.contains(&ty) || self.channels.contains_key(id) {
				continue;
			}
			let parent = match c.get("parent_id").and_then(|v| v.as_str()) {
				Some(p) => match self.channels.get(p) {
					Some(mapped) => Some(mapped.dst_id.clone()),
					// on a dry run its category is two lines up this same plan
					None if dry_run => None,
					None => bail!("`{name}` sits under a category the mirror does not hold: {p}"),
				},
				None => None,
			};

			if dry_run {
				println!("+ _{name}  (type {ty})");
				continue;
			}

			let mut payload = json!({ "name": format!("_{name}"), "type": ty });
			for field in ["position", "topic", "nsfw", "rate_limit_per_user"] {
				if let Some(v) = c.get(field)
					&& !v.is_null()
				{
					payload[field] = v.clone();
				}
			}
			if let Some(parent) = parent {
				payload["parent_id"] = json!(parent);
			}

			let created = self
				.rest
				.create_channel(&self.config.target_guild, &payload)
				.await
				.wrap_err_with(|| format!("creating _{name}"))?;
			let dst = str_of(&created, "id")?.to_string();
			let webhook = match ty == CATEGORY {
				true => None,
				false => Some(self.rest.create_webhook(&dst, "mirror").await?),
			};
			self.db.mirror_channel(id, &dst, webhook.as_deref()).await?;
			self.remember(MirrorChannel {
				src_id: id.to_string(),
				dst_id: dst,
				webhook,
				backfill_cursor: None,
				backfill_done: false,
			});
			info!("mirror: created _{name}");
		}

		self.sync_threads(&source, dry_run).await
	}

	/// After their parents, and posting through the parent's webhook with `?thread_id=` — so a
	/// thread's stored endpoint is the whole of where its messages go.
	async fn sync_threads(&mut self, source: &[serde_json::Value], dry_run: bool) -> Result<()> {
		let mut threads = self.rest.active_threads(&self.config.source_guild).await?;
		for c in source.iter().filter(|c| THREADED.contains(&kind(c))) {
			threads.extend(self.rest.archived_threads(str_of(c, "id")?).await?);
		}
		let source_kinds: HashMap<&str, u64> = source.iter().map(|c| (str_of(c, "id").expect("a discord channel carries an id"), kind(c))).collect();

		for t in &threads {
			let (id, name) = (str_of(t, "id")?, str_of(t, "name")?);
			if self.channels.contains_key(id) {
				continue;
			}
			let Some(parent_src) = t.get("parent_id").and_then(|v| v.as_str()) else {
				continue;
			};
			if dry_run {
				// its parent is on this same plan, so the channel map cannot answer for it yet
				if source_kinds.get(parent_src).is_some_and(|k| MIRRORED.contains(k)) {
					println!("+ ▸_{name}");
				}
				continue;
			}
			let Some(parent) = self.channels.get(parent_src) else {
				continue; // its parent is a type the mirror does not carry
			};
			let hook = parent.webhook.clone().ok_or_else(|| eyre!("thread `{name}` hangs under a category"))?;
			let parent_dst = parent.dst_id.clone();

			let dst = match source_kinds.get(parent_src) == Some(&FORUM) {
				// a forum post *is* a message, so the only way to open the thread is to write one
				true => {
					let opener = json!({ "thread_name": format!("_{name}"), "content": "…", "allowed_mentions": { "parse": [] } });
					str_of(&self.rest.execute_webhook(&hook, &opener, &[]).await?, "channel_id")?.to_string()
				}
				false => str_of(&self.rest.create_thread(&parent_dst, &format!("_{name}")).await?, "id")?.to_string(),
			};
			let webhook = Some(format!("{hook}?thread_id={dst}"));
			self.db.mirror_channel(id, &dst, webhook.as_deref()).await?;
			self.remember(MirrorChannel {
				src_id: id.to_string(),
				dst_id: dst,
				webhook,
				backfill_cursor: None,
				backfill_done: false,
			});
			info!("mirror: created ▸_{name}");
		}
		Ok(())
	}

	async fn tail(&self) -> Result<Infallible, AdapterError> {
		let mut attempt: u32 = 0;
		loop {
			match Gateway::connect(&self.token, SURFACE).await {
				Ok(mut gateway) => {
					info!("mirror: connected to WebSocket");
					attempt = 0;
					while let Some((event_type, d)) = gateway.next().await? {
						if event_type == "MESSAGE_CREATE"
							&& let Err(e) = self.route(&d).await
						{
							error!("mirror: {e:#}");
						}
					}
				}
				Err(e) => error!("mirror: gateway connect failed: {e:#}"),
			}
			let delay = reconnect_delay(attempt);
			warn!("mirror reconnecting in {:.1}s (attempt {attempt})", delay.as_secs_f64());
			time::sleep(delay).await;
			attempt = attempt.saturating_add(1);
		}
	}

	async fn route(&self, m: &serde_json::Value) -> Result<()> {
		let (Some(id), Some(channel), Some(guild)) = (
			m.get("id").and_then(|v| v.as_str()),
			m.get("channel_id").and_then(|v| v.as_str()),
			m.get("guild_id").and_then(|v| v.as_str()),
		) else {
			return Ok(()); // a DM, which the mirror has no business in
		};

		if guild == self.config.source_guild {
			// A forward we posted comes back here as an ordinary source message. `mirror_messages`
			// catches it once the POST has returned; this catches it when the gateway is faster.
			// Its cost: a forward made by hand inside the source guild does not mirror.
			let ours = m.pointer("/author/username").and_then(|v| v.as_str()) == Some(self.my_username.as_str());
			if ours && m.get("message_snapshots").is_some() {
				return Ok(());
			}
			if self.db.map_message(id).await?.is_some() {
				return Ok(());
			}
			let Some(target) = self.channels.get(channel) else {
				return Ok(()); // a channel or thread born since the last topology sync
			};
			let Some(endpoint) = target.webhook.as_deref() else {
				return Ok(());
			};
			self.mirror(m, endpoint, &target.dst_id).await
		} else if guild == self.config.target_guild {
			// everything the mirror writes on our side arrives over a webhook, which is the whole
			// of the loop guard in this direction
			if m.get("webhook_id").is_some() {
				return Ok(());
			}
			let Some(src_channel) = self.back.get(channel) else {
				return Ok(());
			};
			let posted = self.rest.forward(src_channel, &self.config.target_guild, channel, id).await?;
			self.db.mirror_message(str_of(&posted, "id")?, id).await
		} else {
			Ok(())
		}
	}

	async fn mirror(&self, m: &serde_json::Value, endpoint: &str, dst_channel: &str) -> Result<()> {
		let src_id = str_of(m, "id")?;
		let (payload, files) = self.render(m, dst_channel).await?;
		let empty = payload["content"].as_str().is_none_or(str::is_empty) && payload["embeds"].as_array().is_none_or(Vec::is_empty) && files.is_empty();
		if empty {
			// a sticker, a poll, a join notice: real shapes a webhook cannot carry
			warn!("mirror: {src_id} has nothing a webhook can carry");
			return Ok(());
		}
		let posted = self.rest.execute_webhook(endpoint, &payload, &files).await?;
		self.db.mirror_message(src_id, str_of(&posted, "id")?).await
	}

	async fn render(&self, m: &serde_json::Value, dst_channel: &str) -> Result<(serde_json::Value, Vec<(String, Vec<u8>)>)> {
		let author = m.get("author").ok_or_else(|| eyre!("a discord message carries an author"))?;
		let name = author
			.get("global_name")
			.and_then(|v| v.as_str())
			.or_else(|| author.get("username").and_then(|v| v.as_str()))
			.ok_or_else(|| eyre!("a discord author carries a name"))?;

		let mut content = String::new();
		if let Some(replied) = m.pointer("/message_reference/message_id").and_then(|v| v.as_str())
			&& let Some(mapped) = self.db.map_message(replied).await?
		{
			// a webhook cannot set `message_reference`, so a reply becomes a line
			let to = m
				.pointer("/referenced_message/author/global_name")
				.or_else(|| m.pointer("/referenced_message/author/username"))
				.and_then(|v| v.as_str());
			let label = match to {
				Some(to) => format!("↩ **{to}**"),
				None => "↩".to_string(), // the message it answers is gone
			};
			content.push_str(&format!("> [{label}](https://discord.com/channels/{}/{dst_channel}/{mapped})\n", self.config.target_guild));
		}
		content.push_str(m.get("content").and_then(|v| v.as_str()).unwrap_or_default());

		let mut files = Vec::new();
		let mut budget = UPLOAD_LIMIT;
		for a in m.get("attachments").and_then(|v| v.as_array()).into_iter().flatten() {
			let url = str_of(a, "url")?;
			let filename = str_of(a, "filename")?;
			let size = a.get("size").and_then(|v| v.as_u64()).ok_or_else(|| eyre!("a discord attachment carries a size"))?;
			// source cdn urls are signed and expire, so re-upload; over the platform's ceiling the
			// link is all that is left
			if size > budget {
				content.push_str(&format!("\n{url}"));
				continue;
			}
			budget -= size;
			files.push((filename.to_string(), self.rest.download(url).await?));
		}

		if content.chars().count() > CONTENT_LIMIT {
			// ponytail: truncate rather than split; a message this long is rare and the tail is
			// still reachable in the source
			content = content.chars().take(CONTENT_LIMIT - 1).collect::<String>() + "…";
		}

		let mut payload = json!({
			"username": webhook_name(name),
			"content": content,
			"allowed_mentions": { "parse": [] },
		});
		if let Some(embeds) = m.get("embeds") {
			payload["embeds"] = embeds.clone();
		}
		if let (Some(uid), Some(hash)) = (author.get("id").and_then(|v| v.as_str()), author.get("avatar").and_then(|v| v.as_str())) {
			payload["avatar_url"] = json!(format!("https://cdn.discordapp.com/avatars/{uid}/{hash}.png"));
		}
		Ok((payload, files))
	}

	/// Oldest to newest per channel, writing the cursor after every page so a kill resumes rather
	/// than restarts. Webhook posting is rate limited per channel, so a large guild is a wait.
	async fn backfill(&self) -> Result<()> {
		for row in self.db.mirror_channels().await? {
			let Some(endpoint) = row.webhook.as_deref() else {
				continue;
			};
			if row.backfill_done {
				continue;
			}
			let mut after: u64 = match &row.backfill_cursor {
				Some(c) => c.parse().wrap_err("a backfill cursor is a snowflake")?,
				None => 0,
			};
			info!("mirror: backfilling {} from {after}", row.src_id);
			//LOOP: walks strictly upwards from the cursor, and stops on the first page that is not full
			loop {
				let page = self.rest.messages(&row.src_id, Anchor::After(after), PAGE).await?;
				let ids: Vec<u64> = page.iter().map(|(id, _)| *id).collect();
				// newest-first off the wire, and a replay has to arrive in the order it happened
				for (id, m) in page.iter().rev() {
					// one message a webhook rejects must not strand every message above it, and the
					// cursor moves past it either way — so it is reported, loudly, with its id
					if let Err(e) = self.mirror(m, endpoint, &row.dst_id).await {
						error!("mirror: dropped {}/{id} in backfill: {e:#}", row.src_id);
					}
				}
				let highest = ids.iter().copied().max();
				match next_after(&ids, PAGE) {
					Some(next) => {
						self.db.set_backfill_cursor(&row.src_id, Some(&next.to_string()), false).await?;
						after = next;
					}
					None => {
						let cursor = highest.map(|h| h.to_string());
						self.db.set_backfill_cursor(&row.src_id, cursor.as_deref().or(row.backfill_cursor.as_deref()), true).await?;
						break;
					}
				}
			}
		}
		info!("mirror: backfill complete");
		Ok(())
	}
}

/// Discord refuses a webhook username containing either, at any casing.
static BANNED_NAME: LazyLock<Regex> = LazyLock::new(|| Regex::new("(?i)discord|clyde").expect("literal alternation"));

impl Client for DiscordMirror {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		self.sync(false).await.map_err(|e| AdapterError::Unhandled {
			surface: SURFACE,
			detail: format!("topology sync: {e:#}"),
		})?;

		// The account-safety exposure of this surface sits in the backfill, and this is what keeps
		// it human-initiated: under systemd stdin is closed, which reads as `No`.
		let pull = confirmation("pull the entire history?").flush().await == ConfirmResult::Yes;
		let tail = std::pin::pin!(self.tail());
		if !pull {
			return tail.await;
		}
		let backfill = std::pin::pin!(self.backfill());
		match select(tail, backfill).await {
			Either::Left((tail, _)) => tail,
			Either::Right((backfill, tail)) => {
				if let Err(e) = backfill {
					error!("mirror: backfill failed: {e:#}");
				}
				tail.await
			}
		}
	}
}

fn kind(c: &serde_json::Value) -> u64 {
	c.get("type").and_then(|v| v.as_u64()).expect("a discord channel carries a type")
}

fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
	v.get(key).and_then(|v| v.as_str()).ok_or_else(|| eyre!("a discord object carries `{key}`: {v}"))
}

/// The banned words are bent rather than cut: cutting can leave the name empty, which Discord
/// refuses in turn.
fn webhook_name(name: &str) -> String {
	let bent = BANNED_NAME.replace_all(name, |c: &regex::Captures| match c[0].to_lowercase().as_str() {
		"discord" => "disc0rd",
		_ => "clyd3",
	});
	bent.chars().take(80).collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_webhook_name_survives_the_ban() {
		assert_eq!(webhook_name("Discord Nitro"), "disc0rd Nitro");
		assert_eq!(webhook_name("CLYDE"), "clyd3");
		assert_eq!(webhook_name("alice"), "alice");
		assert_eq!(webhook_name(&"a".repeat(100)).chars().count(), 80);
	}
}
