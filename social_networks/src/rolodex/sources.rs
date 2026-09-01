//! The platform access `pull` diffs against and `dm` writes through. Every fetch is scoped to one
//! handle so a handle that has stopped resolving takes only itself down.

use std::{collections::BTreeMap, path::Path, time::Duration};

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use grammers_client::{Client, media::Media, message::Message};
use grammers_tl_types as tl;
use jiff::{Timestamp, tz::TimeZone};
use serde::{Deserialize, Serialize};
use social_networks_utils::skool::Skool;
use strum::{AsRefStr, EnumIter, EnumString};
use tracing::warn;

use super::{avif, history::Cursor};

/// The window the extraction prompt reads, and therefore how much of a conversation the first pull
/// puts in front of it — the history under that is the backfill's, not the prompt's.
pub(super) const INITIAL_MESSAGES: usize = 200;
/// Ceiling per pull once a checkpoint exists. Whatever is left over is picked up by the next run.
const MAX_MESSAGES: usize = 500;
/// Discord's own cap on `limit`; asking for more is a 400, not a bigger page.
const PAGE: usize = 100;
/// A 429 that outlasts this many waits is not a burst.
const RATE_LIMIT_RETRIES: usize = 8;
/// How long a fetched linkedin profile is taken as still current. The anonymous view budget is a
/// handful of profiles before the authwall, so a pull has to touch a few people rather than all of
/// them — which a headline that changes twice a year can afford.
const PROFILE_REFRESH_DAYS: i32 = 30;
const LINKS: [(&str, &str); 5] = [
	("linkTwitter", "twitter"),
	("linkYoutube", "youtube"),
	("linkInstagram", "instagram"),
	("linkLinkedin", "linkedin"),
	("linkFacebook", "facebook"),
];
/// The `handles` keys `pull` fetches. Separating this from the dispatch in `mod` would let a new
/// source fall through to the no-fetch-path arm and silently do nothing.
#[derive(AsRefStr, Clone, Copy, Debug, Deserialize, EnumIter, EnumString, Eq, Hash, PartialEq, Serialize)]
#[strum(serialize_all = "lowercase")]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum Source {
	Discord,
	Telegram,
	Github,
	Linkedin,
	Skool,
}
impl Source {
	/// Whether there is anything below the newest item to page down to. A github feed, a linkedin
	/// profile and a skool post list are snapshots, so their backfill is over before it starts.
	pub fn has_history(self) -> bool {
		matches!(self, Self::Discord | Self::Telegram)
	}
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Msg {
	pub id: String,
	pub source: Source,
	pub at: Timestamp,
	pub outgoing: bool,
	pub text: String,
	pub attachments: Vec<Attachment>,
	pub permalink: Option<String>,
}
/// An image is kept: converted once, under a name its own id determines, so a re-download costs
/// nothing. Everything else is named and not kept — a transcript that says a file went by is worth
/// far more than the bytes of it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub enum Attachment {
	Image { file: String },
	File { name: String },
}
/// Something a person did in public. Kept apart from [`Msg`] because it is worth recording under a
/// far higher bar — see the prompt in `delta`.
pub struct Activity {
	pub date: String,
	pub text: String,
	pub permalink: String,
}
/// What one platform knows about a person right now.
#[derive(Default)]
pub struct Fetched {
	pub sources: BTreeMap<String, String>,
	pub handles: BTreeMap<String, String>,
	/// Oldest-first.
	pub messages: Vec<Msg>,
	/// Oldest-first.
	pub activity: Vec<Activity>,
}
pub struct Discord {
	http: reqwest::Client,
	token: String,
	my_username: String,
}
impl Discord {
	pub fn new(token: String, my_username: String) -> Self {
		Self {
			http: reqwest::Client::new(),
			token,
			my_username,
		}
	}

	/// Only channels that already exist: opening one takes a user id, which nothing outside an open
	/// channel hands us.
	async fn dm_channel(&self, handle: &str) -> Result<(String, String)> {
		let channels: Vec<serde_json::Value> = self.get("https://discord.com/api/v10/users/@me/channels").await?;
		channels
			.iter()
			.find_map(|c| {
				let channel_id = c.get("id")?.as_str()?;
				let recipient = c.get("recipients")?.as_array()?.iter().find(|r| r.get("username").and_then(|u| u.as_str()) == Some(handle))?;
				Some((channel_id.to_string(), recipient.get("id")?.as_str()?.to_string()))
			})
			.ok_or_else(|| eyre!("no discord DM channel with `{handle}`"))
	}

	pub async fn send(&self, handle: &str, text: &str) -> Result<()> {
		let (channel_id, _) = self.dm_channel(handle).await?;
		self.http
			.post(format!("https://discord.com/api/v10/channels/{channel_id}/messages"))
			.header("authorization", &self.token)
			.json(&serde_json::json!({ "content": text }))
			.send()
			.await?
			.error_for_status()?;
		Ok(())
	}

	pub async fn fetch(&self, handle: &str, cursor: &mut Cursor<'_>, assets: &Path) -> Result<Fetched> {
		let (channel_id, user_id) = self.dm_channel(handle).await?;

		let mut fetched = Fetched::default();

		// 404 here means no note is set, not that the user is gone.
		let note = self.request(|| self.http.get(format!("https://discord.com/api/v10/users/@me/notes/{user_id}"))).await?;
		if note.status() != reqwest::StatusCode::NOT_FOUND {
			let note: serde_json::Value = note.error_for_status()?.json().await?;
			insert_nonempty(&mut fetched.sources, "discord:note", note.get("note").and_then(|v| v.as_str()));
		}

		let profile: serde_json::Value = self.get(&format!("https://discord.com/api/v10/users/{user_id}/profile")).await?;
		// `/user_profile/bio` carries the same text; it only diverges per-guild, which `@me` never is
		insert_nonempty(&mut fetched.sources, "discord:bio", profile.pointer("/user/bio").and_then(|v| v.as_str()));
		insert_nonempty(&mut fetched.sources, "discord:pronouns", profile.pointer("/user_profile/pronouns").and_then(|v| v.as_str()));
		for account in profile.get("connected_accounts").and_then(|v| v.as_array()).into_iter().flatten() {
			if let (Some(kind), Some(name)) = (account.get("type").and_then(|v| v.as_str()), account.get("name").and_then(|v| v.as_str())) {
				fetched.handles.insert(kind.to_string(), name.to_string());
			}
		}

		let raw = match cursor.newest() {
			// walk backwards from the newest, since there is no floor to walk up from
			None => {
				let mut raw: Vec<(u64, serde_json::Value)> = Vec::new();
				let mut anchor = Anchor::Newest;
				while raw.len() < INITIAL_MESSAGES {
					let page = self.messages(&channel_id, anchor, PAGE.min(INITIAL_MESSAGES - raw.len())).await?;
					let Some(oldest) = page.iter().map(|(id, _)| *id).min() else { break };
					let short = page.len() < PAGE;
					raw.extend(page);
					if short {
						break;
					}
					anchor = Anchor::Before(oldest);
				}
				raw
			}
			Some(newest) => {
				let mut anchor = Anchor::After(newest.parse().wrap_err("a discord checkpoint is a snowflake")?);
				let mut raw = Vec::new();
				loop {
					let page = self.messages(&channel_id, anchor, PAGE).await?;
					let ids: Vec<u64> = page.iter().map(|(id, _)| *id).collect();
					raw.extend(page);
					if raw.len() >= MAX_MESSAGES {
						warn!("discord `{handle}`: stopping at {MAX_MESSAGES} messages, the rest comes on the next pull");
						break;
					}
					match next_after(&ids, PAGE) {
						Some(next) => anchor = Anchor::After(next),
						None => break,
					}
				}
				raw
			}
		};

		if let Some(newest) = raw.iter().map(|(id, _)| *id).max() {
			cursor.advance(newest.to_string());
		}
		let floor = raw.iter().map(|(id, _)| *id).min();
		let mut messages = Vec::with_capacity(raw.len());
		for (id, m) in &raw {
			messages.push(self.message(&channel_id, *id, m, assets).await?);
		}
		messages.sort_by_key(|m| m.at);
		fetched.messages = messages;

		// before the backfill, not after it: a page checked in below this slice persists the cursor
		// above it, and a kill in between would leave the slice in no file at all
		if cursor.archiving() {
			cursor.stash(&fetched.messages)?;
		}
		if cursor.backfilling() {
			self.backfill(&channel_id, cursor, assets, floor).await?;
		}
		Ok(fetched)
	}

	/// Down to the first message of the conversation, checking every page in before asking for the
	/// next — so an interrupt costs the page in flight and nothing behind it.
	async fn backfill(&self, channel_id: &str, cursor: &mut Cursor<'_>, assets: &Path, incremental_floor: Option<u64>) -> Result<()> {
		let mut anchor = match cursor.floor() {
			Some(floor) => Anchor::Before(floor.parse().wrap_err("a discord backfill floor is a snowflake")?),
			// the slice fetched above is already accounted for, so the walk starts under it
			None => incremental_floor.map_or(Anchor::Newest, Anchor::Before),
		};
		//LOOP: bounded by the conversation, which is finite and walked strictly downwards — the short
		// page that ends it cannot be predicted from the page before it
		loop {
			let page = self.messages(channel_id, anchor, PAGE).await?;
			let Some(oldest) = page.iter().map(|(id, _)| *id).min() else { break };
			let short = page.len() < PAGE;
			let mut msgs = Vec::with_capacity(page.len());
			for (id, m) in &page {
				msgs.push(self.message(channel_id, *id, m, assets).await?);
			}
			cursor.page(&msgs, oldest.to_string())?;
			if short {
				break;
			}
			anchor = Anchor::Before(oldest);
		}
		cursor.exhausted()
	}

	async fn messages(&self, channel_id: &str, anchor: Anchor, limit: usize) -> Result<Vec<(u64, serde_json::Value)>> {
		assert!(limit <= PAGE, "discord rejects a limit above {PAGE}");
		let mut query = vec![("limit".to_string(), limit.to_string())];
		match anchor {
			Anchor::Newest => {}
			Anchor::After(id) => query.push(("after".to_string(), id.to_string())),
			Anchor::Before(id) => query.push(("before".to_string(), id.to_string())),
		}
		let page: Vec<serde_json::Value> = self
			.request(|| self.http.get(format!("https://discord.com/api/v10/channels/{channel_id}/messages")).query(&query))
			.await?
			.error_for_status()?
			.json()
			.await?;
		page.into_iter()
			.map(|m| {
				let id = m.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("discord message without an id"))?;
				Ok((id.parse().wrap_err("discord ids are snowflakes")?, m))
			})
			.collect()
	}

	async fn message(&self, channel_id: &str, id: u64, m: &serde_json::Value, assets: &Path) -> Result<Msg> {
		let timestamp = m.get("timestamp").and_then(|v| v.as_str()).ok_or_else(|| eyre!("discord message {id} without a timestamp"))?;
		Ok(Msg {
			id: id.to_string(),
			source: Source::Discord,
			at: timestamp.parse().wrap_err("discord timestamps are RFC3339")?,
			outgoing: m.pointer("/author/username").and_then(|v| v.as_str()) == Some(&self.my_username),
			text: m.get("content").and_then(|v| v.as_str()).unwrap_or_default().to_string(), // an attachment-only message carries no content and is still worth its date
			attachments: self.attachments(m, assets).await?,
			permalink: Some(format!("https://discord.com/channels/@me/{channel_id}/{id}")),
		})
	}

	async fn attachments(&self, m: &serde_json::Value, assets: &Path) -> Result<Vec<Attachment>> {
		let mut out = Vec::new();
		for attachment in m.get("attachments").and_then(|v| v.as_array()).into_iter().flatten() {
			let name = attachment
				.get("filename")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("a discord attachment carries a filename"))?
				.to_string();
			let id = attachment.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a discord attachment carries an id"))?;
			let file = format!("discord-{id}.avif");
			if assets.join(&file).exists() {
				out.push(Attachment::Image { file });
				continue;
			}
			// an undeclared content type is one discord itself would not call an image
			let mime = attachment.get("content_type").and_then(|v| v.as_str()).unwrap_or_default();
			if !mime.starts_with("image/") {
				out.push(Attachment::File { name });
				continue;
			}

			let url = attachment.get("url").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a discord attachment carries a url"))?;
			// the cdn url is signed; the token belongs on the api, not on it
			let bytes = self.http.get(url).send().await?.error_for_status()?.bytes().await?;
			out.push(keep(&bytes, mime, name, &assets.join(&file), file.clone()));
		}
		Ok(out)
	}

	/// Discord answers a burst with a 429 and a body saying how long to hold off. A backfill is
	/// hundreds of requests, so meeting one is expected rather than exceptional.
	async fn request(&self, build: impl Fn() -> reqwest::RequestBuilder) -> Result<reqwest::Response> {
		for _ in 0..RATE_LIMIT_RETRIES {
			let response = build().header("authorization", &self.token).send().await?;
			if response.status() != reqwest::StatusCode::TOO_MANY_REQUESTS {
				return Ok(response);
			}
			let body: serde_json::Value = response.json().await?;
			let after = body
				.get("retry_after")
				.and_then(|v| v.as_f64())
				.ok_or_else(|| eyre!("a discord 429 carries `retry_after`: {body}"))?;
			warn!("discord: rate limited, holding off {after}s");
			tokio::time::sleep(Duration::from_secs_f64(after)).await;
		}
		Err(eyre!("discord: still rate limited after {RATE_LIMIT_RETRIES} waits"))
	}

	async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
		Ok(self.request(|| self.http.get(url)).await?.error_for_status()?.json().await?)
	}
}

pub async fn telegram_send(client: &Client, handle: &str, text: &str) -> Result<()> {
	client.send_message(telegram_peer(client, handle).await?, text).await?;
	Ok(())
}
pub async fn telegram(client: &Client, handle: &str, cursor: &mut Cursor<'_>, assets: &Path) -> Result<Fetched> {
	let peer_ref = telegram_peer(client, handle).await?;

	let mut fetched = Fetched::default();

	let tl::enums::users::UserFull::Full(full) = client.invoke(&tl::functions::users::GetFullUser { id: peer_ref.into() }).await?;
	let tl::enums::UserFull::Full(user_full) = full.full_user;
	insert_nonempty(&mut fetched.sources, "telegram:about", user_full.about.as_deref());

	let newest: Option<i32> = cursor.newest().map(|c| c.parse()).transpose().wrap_err("a telegram checkpoint is a message id")?;
	let limit = if newest.is_some() { MAX_MESSAGES } else { INITIAL_MESSAGES };
	let mut messages = Vec::new();
	let mut iter = client.iter_messages(peer_ref);
	// newest-first
	while let Some(message) = iter.next().await? {
		if newest.is_some_and(|c| message.id() <= c) {
			break;
		}
		if messages.is_empty() {
			cursor.advance(message.id().to_string());
		}
		messages.push(telegram_msg(client, &message, assets).await?);
		if messages.len() >= limit {
			warn!("telegram `{handle}`: stopping at {limit} messages, the rest comes on the next pull");
			break;
		}
	}
	let floor = messages.iter().map(|m| m.id.parse::<i32>().expect("a telegram id we just printed")).min();
	messages.reverse();
	fetched.messages = messages;

	// see the same call in `Discord::fetch`
	if cursor.archiving() {
		cursor.stash(&fetched.messages)?;
	}
	if cursor.backfilling() {
		telegram_backfill(client, peer_ref, cursor, assets, floor).await?;
	}
	Ok(fetched)
}
/// Unauthenticated: everything read here is public, and 60 requests an hour is far more than a
/// hand-run pull over a rolodex spends. A rate-limit 403 surfaces as this handle's failure.
#[derive(Default)]
pub struct Github {
	http: reqwest::Client,
}
impl Github {
	pub async fn fetch(&self, handle: &str, cursor: &mut Cursor<'_>) -> Result<Fetched> {
		let mut fetched = Fetched::default();

		let profile: serde_json::Value = self.get(&format!("https://api.github.com/users/{handle}")).await?;
		insert_nonempty(&mut fetched.sources, "github:bio", profile.get("bio").and_then(|v| v.as_str()));

		// The feed only reaches back 300 events / 90 days no matter how it is paged, so one page is
		// the whole of what a rare pull could have recovered anyway.
		let events: Vec<serde_json::Value> = self.get(&format!("https://api.github.com/users/{handle}/events/public?per_page={PAGE}")).await?;
		let full_page = events.len() == PAGE;
		let newest: Option<u64> = cursor.newest().map(|c| c.parse()).transpose().wrap_err("a github checkpoint is an event id")?;

		let mut reached_cursor = false;
		let mut first = true;
		// newest-first
		for event in &events {
			let id: u64 = event
				.get("id")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("github event without an id"))?
				.parse()
				.wrap_err("github event ids are numeric")?;
			if newest.is_some_and(|c| id <= c) {
				reached_cursor = true;
				break;
			}
			if std::mem::take(&mut first) {
				cursor.advance(id.to_string());
			}
			let Some((text, permalink)) = describe(event) else {
				continue;
			};
			let date = event.get("created_at").and_then(|v| v.as_str()).ok_or_else(|| eyre!("github event {id} without created_at"))?;
			fetched.activity.push(Activity {
				date: date.parse::<Timestamp>().wrap_err("github timestamps are RFC3339")?.to_zoned(TimeZone::UTC).date().to_string(),
				text,
				permalink,
			});
		}
		if full_page && !reached_cursor {
			warn!("github `{handle}`: the whole {PAGE}-event page was new, anything older is past what the feed keeps");
		}
		fetched.activity.reverse();
		Ok(fetched)
	}

	async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
		// github 403s a request without one
		Ok(self
			.http
			.get(url)
			.header("user-agent", "social_networks-rolodex")
			.send()
			.await?
			.error_for_status()?
			.json()
			.await?)
	}
}

/// Logged out, so no credentials of any kind — and therefore no messages and no post feed, only the
/// one fact no other source states: where a person works now.
///
/// The checkpoint is the date of the last success rather than an item id: there is nothing to page
/// through, only a snapshot to re-take once it is old enough to be worth a view from the budget.
///
/// Through `curl` rather than the http client every other source shares, because linkedin answers on
/// the TLS handshake as much as on the request: reqwest gets `999` where a curl carrying byte-identical
/// headers gets the page.
pub fn linkedin(handle: &str, cursor: &mut Cursor<'_>) -> Result<Fetched> {
	let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
	if let Some(last) = cursor.newest() {
		let last: jiff::civil::Date = last.parse().wrap_err("a linkedin checkpoint is a date")?;
		// an early return leaves the checkpoint where it is, so the skip is not itself a success
		if last.until((jiff::Unit::Day, today))?.get_days() < PROFILE_REFRESH_DAYS {
			return Ok(Fetched::default());
		}
	}

	// `%{stderr}` keeps the status code out of the body, so the wall is a code rather than a guess
	let out = std::process::Command::new("curl")
		.args([
			"-sL",
			"--max-time",
			"30",
			"-w",
			"%{stderr}%{http_code}",
			"-A",
			"Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
			&format!("https://www.linkedin.com/in/{handle}/"),
		])
		.output()
		.wrap_err("failed to run `curl`")?;
	if !out.status.success() {
		bail!("curl {}", out.status);
	}
	let code = String::from_utf8_lossy(&out.stderr);
	if code.trim() != "200" {
		bail!("linkedin `{handle}`: HTTP {} — `999` is the wall, and it lifts on its own", code.trim());
	}

	let person = person_node(&String::from_utf8_lossy(&out.stdout)).wrap_err_with(|| format!("linkedin `{handle}`"))?;
	let mut fetched = Fetched::default();
	insert_nonempty(&mut fetched.sources, "linkedin:headline", Some(&headline(&person)));
	insert_nonempty(&mut fetched.sources, "linkedin:about", person.get("description").and_then(|v| v.as_str()));
	cursor.advance(today.to_string());
	Ok(fetched)
}
/// The profile fields skool serves to anybody. Their absence of a session is why this is the one
/// source that needs no credentials at all: `postTrees` is the only part membership adds, and it
/// comes back empty rather than failing.
pub async fn skool(client: &mut Skool, handle: &str, cursor: &mut Cursor<'_>) -> Result<Fetched> {
	let handle = handle.trim_start_matches('@');
	let payload = client.page(&format!("/@{handle}")).await?;
	let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("skool served a page without pageProps"))?;
	let user = props.get("currentUser").ok_or_else(|| eyre!("no such skool handle: `{handle}`"))?;

	let mut fetched = Fetched::default();
	let metadata = user.get("metadata");
	let field = |name: &str| metadata.and_then(|m| m.get(name)).and_then(|v| v.as_str());
	insert_nonempty(&mut fetched.sources, "skool:bio", field("bio"));
	insert_nonempty(&mut fetched.sources, "skool:location", field("location"));
	// read-only for a human, exactly like discord's connected accounts: none of these is a fetchable `Source`
	for (link, platform) in LINKS {
		if let Some(name) = field(link).and_then(handle_from_link) {
			fetched.handles.insert(platform.to_string(), name);
		}
	}

	// newest-first, and only ever populated for a session that shares a group with them
	let posts = props.get("postTrees").and_then(|v| v.as_array()).ok_or_else(|| eyre!("skool `{handle}`: no postTrees"))?;
	let mut first = true;
	for node in posts {
		let post = node.get("post").ok_or_else(|| eyre!("a skool postTree without a post: {node}"))?;
		let id = post.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool post without an id: {post}"))?;
		// ids are opaque hex, so the cursor can only be recognised, not compared — a post that has
		// already scrolled off the first page is reported again rather than missed
		if cursor.newest() == Some(id) {
			break;
		}
		if std::mem::take(&mut first) {
			cursor.advance(id.to_string());
		}
		let created = post.get("createdAt").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without createdAt"))?;
		let title = post.pointer("/metadata/title").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without a title"))?;
		let (group, name) = (post.pointer("/group/name").and_then(|v| v.as_str()), post.get("name").and_then(|v| v.as_str()));
		let (Some(group), Some(name)) = (group, name) else {
			bail!("skool post {id} carries no group/name to build a permalink from: {post}");
		};
		fetched.activity.push(Activity {
			date: created.parse::<Timestamp>().wrap_err("skool timestamps are RFC3339")?.to_zoned(TimeZone::UTC).date().to_string(),
			text: title.to_string(),
			permalink: format!("https://www.skool.com/{group}/{name}"),
		});
	}
	fetched.activity.reverse();
	Ok(fetched)
}
/// An image that will not convert is still an attachment that went by, and losing the whole page of
/// a backfill over one of them would cost the conversation around it.
fn keep(bytes: &[u8], mime: &str, name: String, dest: &Path, file: String) -> Attachment {
	if !avif::still(mime, bytes) {
		return Attachment::File { name };
	}
	match avif::convert(bytes, dest) {
		Ok(()) => Attachment::Image { file },
		Err(e) => {
			warn!("`{name}` stays a filename: {e:#}");
			Attachment::File { name }
		}
	}
}

/// `offset_id` walks strictly older, and `0` means "from the newest" — the same two anchors discord
/// pages by, spelled once.
async fn telegram_backfill(client: &Client, peer_ref: grammers_client::session::types::PeerRef, cursor: &mut Cursor<'_>, assets: &Path, incremental_floor: Option<i32>) -> Result<()> {
	let mut offset = match cursor.floor() {
		Some(floor) => floor.parse().wrap_err("a telegram backfill floor is a message id")?,
		None => incremental_floor.unwrap_or(0),
	};
	//LOOP: as in `Discord::backfill`
	loop {
		let mut iter = client.iter_messages(peer_ref).offset_id(offset);
		let mut page = Vec::new();
		while page.len() < PAGE {
			let Some(message) = iter.next().await? else { break };
			page.push(message);
		}
		let Some(oldest) = page.iter().map(|m| m.id()).min() else { break };
		let short = page.len() < PAGE;
		let mut msgs = Vec::with_capacity(page.len());
		for message in &page {
			msgs.push(telegram_msg(client, message, assets).await?);
		}
		cursor.page(&msgs, oldest.to_string())?;
		if short {
			break;
		}
		offset = oldest;
	}
	cursor.exhausted()
}

async fn telegram_msg(client: &Client, message: &Message, assets: &Path) -> Result<Msg> {
	Ok(Msg {
		id: message.id().to_string(),
		source: Source::Telegram,
		at: Timestamp::from_second(message.date().timestamp()).wrap_err("a telegram message date is a unix second")?,
		outgoing: message.outgoing(),
		text: message.text().to_string(),
		attachments: telegram_attachment(client, message, assets).await?,
		// telegram DMs have no per-message URL
		permalink: None,
	})
}

/// One media per message: an album arrives as several messages, each carrying its own.
async fn telegram_attachment(client: &Client, message: &Message, assets: &Path) -> Result<Vec<Attachment>> {
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
	let mut download = client.iter_download(&media);
	while let Some(chunk) = download.next().await? {
		bytes.extend_from_slice(&chunk);
	}
	Ok(vec![keep(&bytes, &mime, name, &assets.join(&file), file.clone())])
}

async fn telegram_peer(client: &Client, handle: &str) -> Result<grammers_client::session::types::PeerRef> {
	let peer = client.resolve_username(handle).await?.ok_or_else(|| eyre!("no such telegram username: `{handle}`"))?;
	peer.to_ref().await.map_err(|e| eyre!("{e}"))?.ok_or_else(|| eyre!("`{handle}` resolved but has no usable ref"))
}

/// The last path segment of a profile URL, which is the handle on every platform skool links to.
/// `None` for the empty string skool stores for a link nobody set, and for a bare domain.
fn handle_from_link(url: &str) -> Option<String> {
	let path = url.split(['?', '#']).next().expect("a split yields at least one piece");
	let segment = path.trim_end_matches('/').rsplit('/').next().expect("a split yields at least one piece");
	(!segment.is_empty() && !segment.contains('.')).then(|| segment.to_string())
}

#[derive(Clone, Copy)]
enum Anchor {
	Newest,
	After(u64),
	Before(u64),
}

/// `None` for the event types that carry no signal about a person. Filtering here rather than in the
/// prompt is what keeps the routine churn of a public feed out of the extraction entirely.
fn describe(event: &serde_json::Value) -> Option<(String, String)> {
	let repo = event.pointer("/repo/name")?.as_str()?;
	let repo_url = format!("https://github.com/{repo}");
	let payload = event.get("payload")?;
	match event.get("type")?.as_str()? {
		"PushEvent" => {
			let head = payload.pointer("/commits/0/message").and_then(|v| v.as_str()).unwrap_or("").lines().next().unwrap_or("");
			Some((format!("pushed to {repo}: {head}"), repo_url))
		}
		// a branch or tag create is routine; a repository create is a new project
		"CreateEvent" if payload.get("ref_type").and_then(|v| v.as_str()) == Some("repository") => Some((format!("created repository {repo}"), repo_url)),
		"ReleaseEvent" => {
			let tag = payload.pointer("/release/tag_name").and_then(|v| v.as_str()).unwrap_or("");
			let url = payload.pointer("/release/html_url").and_then(|v| v.as_str()).unwrap_or(&repo_url);
			Some((format!("released {tag} of {repo}"), url.to_string()))
		}
		"PublicEvent" => Some((format!("open-sourced {repo}"), repo_url)),
		"WatchEvent" => Some((format!("starred {repo}"), repo_url)),
		"ForkEvent" => Some((format!("forked {repo}"), repo_url)),
		"PullRequestEvent" => {
			let action = payload.get("action")?.as_str()?;
			let merged = payload.pointer("/pull_request/merged").and_then(|v| v.as_bool()) == Some(true);
			let verb = match (action, merged) {
				("opened", _) => "opened",
				("closed", true) => "merged",
				_ => return None,
			};
			let title = payload.pointer("/pull_request/title").and_then(|v| v.as_str()).unwrap_or("");
			let url = payload.pointer("/pull_request/html_url").and_then(|v| v.as_str()).unwrap_or(&repo_url);
			Some((format!("{verb} a pull request on {repo}: {title}"), url.to_string()))
		}
		_ => None,
	}
}

/// A public profile ships as an ld+json `@graph`; everything around it is obfuscated markup that
/// changes far more often than the schema does. An authwalled page has no `Person` in it — erroring
/// here rather than returning nothing is what keeps a wall distinguishable from an unchanged profile.
fn person_node(body: &str) -> Result<serde_json::Value> {
	const OPEN: &str = r#"<script type="application/ld+json">"#;
	for block in body.split(OPEN).skip(1) {
		let end = block.find("</script>").ok_or_else(|| eyre!("unterminated ld+json block"))?;
		let value: serde_json::Value = serde_json::from_str(&block[..end]).wrap_err("ld+json block is not json")?;
		let person = value
			.get("@graph")
			.and_then(|v| v.as_array())
			.into_iter()
			.flatten()
			.find(|n| n.get("@type").and_then(|v| v.as_str()) == Some("Person"));
		if let Some(person) = person {
			return Ok(person.clone());
		}
	}
	bail!("no ld+json Person: authwalled, or the profile is not public");
}

/// The current role only: the graph carries the whole position history, newest first, and the tail of
/// it is a CV rather than the one fact worth diffing. A title without a company is still worth having,
/// and so is the reverse.
fn headline(person: &serde_json::Value) -> String {
	/// A logged-out view withholds a value by starring it out rather than omitting the field, and how
	/// much it withholds varies with how much it has already served — but no real title carries a `*`.
	fn unmasked(value: Option<&serde_json::Value>) -> Option<&str> {
		value?.as_str().map(str::trim).filter(|v| !v.is_empty() && !v.contains('*'))
	}
	// ld+json spells one value and a list of them the same way
	let title = match person.get("jobTitle") {
		Some(serde_json::Value::Array(titles)) => unmasked(titles.first()),
		title => unmasked(title),
	};
	match (title, unmasked(person.pointer("/worksFor/0/name"))) {
		(Some(title), Some(org)) => format!("{title} at {org}"),
		(title, org) => title.or(org).unwrap_or_default().to_string(),
	}
}

fn insert_nonempty(map: &mut BTreeMap<String, String>, key: &str, value: Option<&str>) {
	if let Some(value) = value.map(str::trim).filter(|v| !v.is_empty()) {
		map.insert(key.to_string(), value.to_string());
	}
}

/// `after=<id>` returns the oldest window past the cursor, newest-first inside it — so the next
/// cursor is the page's largest id, and a short page is the last one.
fn next_after(ids: &[u64], limit: usize) -> Option<u64> {
	(ids.len() == limit).then(|| ids.iter().copied().max().expect("a full page is non-empty"))
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The two event types that share a shape with something far less interesting: a branch create
	/// is not a new project, and a closed pull request is not a merged one.
	#[test]
	fn describe_does_not_inflate() {
		let create = |ref_type| serde_json::json!({"type": "CreateEvent", "repo": {"name": "o/r"}, "payload": {"ref_type": ref_type}});
		assert!(describe(&create("branch")).is_none());
		assert!(describe(&create("repository")).is_some());

		let closed = |merged| serde_json::json!({"type": "PullRequestEvent", "repo": {"name": "o/r"}, "payload": {"action": "closed", "pull_request": {"merged": merged, "title": "t"}}});
		assert!(describe(&closed(false)).is_none());
		assert_eq!(describe(&closed(true)).unwrap().0, "merged a pull request on o/r: t");
	}

	/// A variant whose name does not round-trip is one `handles` cannot address, and it would never be
	/// fetched rather than fail.
	#[test]
	fn every_source_is_addressable() {
		use strum::IntoEnumIterator as _;
		for source in Source::iter() {
			assert!(
				matches!(source.as_ref().parse::<Source>(), Ok(parsed) if parsed.as_ref() == source.as_ref()),
				"{}",
				source.as_ref()
			);
		}
	}

	/// The half that matters is the second: a parser that returns nothing on a wall or a reshaped page
	/// is indistinguishable from an unchanged profile, and would freeze the source without a sound.
	#[test]
	fn linkedin_wall_is_not_silence() {
		let public = r##"<html><head><script type="application/ld+json">{"@context":"http://schema.org","@graph":[{"@type":"WebPage","url":"https://www.linkedin.com/in/x"},{"@type":"Person","name":"X","jobTitle":["Staff Engineer","Intern"],"worksFor":[{"@type":"Organization","name":"Bar"},{"@type":"Organization","name":"Foo"}],"description":"Builds things."}]}</script></head><body></body></html>"##;
		let person = person_node(public).unwrap();
		assert_eq!(headline(&person), "Staff Engineer at Bar");
		assert_eq!(person.get("description").unwrap(), "Builds things.");

		let masked = serde_json::json!({"jobTitle": ["******** *** ***"], "worksFor": [{"name": "Bar"}]});
		assert_eq!(headline(&masked), "Bar");

		let walled = r##"<html><head><script type="application/ld+json">{"@context":"http://schema.org","@graph":[{"@type":"WebPage","url":"https://www.linkedin.com/authwall"}]}</script></head><body>Sign in</body></html>"##;
		assert!(person_node(walled).is_err());
		assert!(person_node("<html><body>Sign in</body></html>").is_err());
	}

	/// Skool stores an unset link as `""` rather than omitting it, and writes the ones it does hold
	/// back in whatever shape the person pasted.
	#[test]
	fn a_link_is_not_a_handle() {
		assert_eq!(handle_from_link(""), None);
		assert_eq!(handle_from_link("https://twitter.com"), None);
		assert_eq!(handle_from_link("https://x.com/valeratrades/"), Some("valeratrades".to_string()));
		assert_eq!(handle_from_link("https://www.youtube.com/@skool-news?sub_confirmation=1"), Some("@skool-news".to_string()));
		assert_eq!(handle_from_link("https://www.linkedin.com/in/somebody#about"), Some("somebody".to_string()));
	}

	#[test]
	fn cursor_walks_forward() {
		assert_eq!(next_after(&[30, 20, 10], 3), Some(30));
		assert_eq!(next_after(&[20, 10], 3), None);
		assert_eq!(next_after(&[], 3), None);
	}
}
