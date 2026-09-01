//! The platform access `pull` diffs against and `dm` writes through. Every fetch is scoped to one
//! handle so a handle that has stopped resolving takes only itself down.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use grammers_client::Client;
use grammers_tl_types as tl;
use jiff::{Timestamp, tz::TimeZone};
use strum::{AsRefStr, EnumIter, EnumString};
use tracing::warn;

/// How far back to reach on the very first pull of a conversation. Without a checkpoint there is no
/// natural stopping point, and a decade of DMs is neither affordable nor interesting — but the first
/// pull is the one that writes the summary from nothing, so it is worth more than a thin slice.
const INITIAL_MESSAGES: usize = 200;
/// Ceiling per pull once a checkpoint exists. Whatever is left over is picked up by the next run.
const MAX_MESSAGES: usize = 500;
/// Discord's own cap on `limit`; asking for more is a 400, not a bigger page.
const PAGE: usize = 100;
/// How long a fetched linkedin profile is taken as still current. The anonymous view budget is a
/// handful of profiles before the authwall, so a pull has to touch a few people rather than all of
/// them — which a headline that changes twice a year can afford.
const PROFILE_REFRESH_DAYS: i32 = 30;
/// The `handles` keys `pull` fetches. Separating this from the dispatch in `mod` would let a new
/// source fall through to the no-fetch-path arm and silently do nothing.
#[derive(AsRefStr, Clone, Copy, Debug, EnumIter, EnumString)]
#[strum(serialize_all = "lowercase")]
#[non_exhaustive]
pub enum Source {
	Discord,
	Telegram,
	Github,
	Linkedin,
}

pub struct Msg {
	pub date: String,
	pub outgoing: bool,
	pub text: String,
	pub permalink: Option<String>,
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
	/// Newest item id seen. `None` when nothing came back, which leaves the checkpoint alone.
	pub cursor: Option<String>,
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

	pub async fn fetch(&self, handle: &str, cursor: Option<&str>) -> Result<Fetched> {
		let (channel_id, user_id) = self.dm_channel(handle).await?;

		let mut fetched = Fetched::default();

		// 404 here means no note is set, not that the user is gone.
		let note = self
			.http
			.get(format!("https://discord.com/api/v10/users/@me/notes/{user_id}"))
			.header("authorization", &self.token)
			.send()
			.await?;
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

		let raw = match cursor {
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
			Some(cursor) => {
				let mut anchor = Anchor::After(cursor.parse().wrap_err("a discord checkpoint is a snowflake")?);
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

		fetched.cursor = raw.iter().map(|(id, _)| *id).max().map(|id| id.to_string());
		let mut messages: Vec<Msg> = raw.into_iter().map(|(id, m)| self.message(&channel_id, id, &m)).collect::<Result<_>>()?;
		messages.sort_by(|a, b| a.date.cmp(&b.date));
		fetched.messages = messages;
		Ok(fetched)
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
			.http
			.get(format!("https://discord.com/api/v10/channels/{channel_id}/messages"))
			.query(&query)
			.header("authorization", &self.token)
			.send()
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

	fn message(&self, channel_id: &str, id: u64, m: &serde_json::Value) -> Result<Msg> {
		let timestamp = m.get("timestamp").and_then(|v| v.as_str()).ok_or_else(|| eyre!("discord message {id} without a timestamp"))?;
		Ok(Msg {
			date: timestamp
				.parse::<Timestamp>()
				.wrap_err("discord timestamps are RFC3339")?
				.to_zoned(TimeZone::UTC)
				.date()
				.to_string(),
			outgoing: m.pointer("/author/username").and_then(|v| v.as_str()) == Some(&self.my_username),
			text: m.get("content").and_then(|v| v.as_str()).unwrap_or_default().to_string(), // an attachment-only message carries no content and is still worth its date
			permalink: Some(format!("https://discord.com/channels/@me/{channel_id}/{id}")),
		})
	}

	async fn get<T: serde::de::DeserializeOwned>(&self, url: &str) -> Result<T> {
		Ok(self.http.get(url).header("authorization", &self.token).send().await?.error_for_status()?.json().await?)
	}
}

pub async fn telegram_send(client: &Client, handle: &str, text: &str) -> Result<()> {
	client.send_message(telegram_peer(client, handle).await?, text).await?;
	Ok(())
}

pub async fn telegram(client: &Client, handle: &str, cursor: Option<&str>) -> Result<Fetched> {
	let peer_ref = telegram_peer(client, handle).await?;

	let mut fetched = Fetched::default();

	let tl::enums::users::UserFull::Full(full) = client.invoke(&tl::functions::users::GetFullUser { id: peer_ref.into() }).await?;
	let tl::enums::UserFull::Full(user_full) = full.full_user;
	insert_nonempty(&mut fetched.sources, "telegram:about", user_full.about.as_deref());

	let cursor: Option<i32> = cursor.map(|c| c.parse()).transpose().wrap_err("a telegram checkpoint is a message id")?;
	let limit = if cursor.is_some() { MAX_MESSAGES } else { INITIAL_MESSAGES };
	let mut messages = Vec::new();
	let mut iter = client.iter_messages(peer_ref);
	// newest-first
	while let Some(message) = iter.next().await? {
		if cursor.is_some_and(|c| message.id() <= c) {
			break;
		}
		fetched.cursor.get_or_insert_with(|| message.id().to_string());
		messages.push(Msg {
			date: message.date().date_naive().to_string(),
			outgoing: message.outgoing(),
			// telegram DMs have no per-message URL
			permalink: None,
			text: message.text().to_string(),
		});
		if messages.len() >= limit {
			warn!("telegram `{handle}`: stopping at {limit} messages, the rest comes on the next pull");
			break;
		}
	}
	messages.reverse();
	fetched.messages = messages;
	Ok(fetched)
}
/// Unauthenticated: everything read here is public, and 60 requests an hour is far more than a
/// hand-run pull over a rolodex spends. A rate-limit 403 surfaces as this handle's failure.
#[derive(Default)]
pub struct Github {
	http: reqwest::Client,
}
impl Github {
	pub async fn fetch(&self, handle: &str, cursor: Option<&str>) -> Result<Fetched> {
		let mut fetched = Fetched::default();

		let profile: serde_json::Value = self.get(&format!("https://api.github.com/users/{handle}")).await?;
		insert_nonempty(&mut fetched.sources, "github:bio", profile.get("bio").and_then(|v| v.as_str()));

		// The feed only reaches back 300 events / 90 days no matter how it is paged, so one page is
		// the whole of what a rare pull could have recovered anyway.
		let events: Vec<serde_json::Value> = self.get(&format!("https://api.github.com/users/{handle}/events/public?per_page={PAGE}")).await?;
		let full_page = events.len() == PAGE;
		let cursor: Option<u64> = cursor.map(|c| c.parse()).transpose().wrap_err("a github checkpoint is an event id")?;

		let mut reached_cursor = false;
		// newest-first
		for event in &events {
			let id: u64 = event
				.get("id")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("github event without an id"))?
				.parse()
				.wrap_err("github event ids are numeric")?;
			if cursor.is_some_and(|c| id <= c) {
				reached_cursor = true;
				break;
			}
			fetched.cursor.get_or_insert_with(|| id.to_string());
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
pub fn linkedin(handle: &str, cursor: Option<&str>) -> Result<Fetched> {
	let today = Timestamp::now().to_zoned(TimeZone::UTC).date();
	if let Some(cursor) = cursor {
		let last: jiff::civil::Date = cursor.parse().wrap_err("a linkedin checkpoint is a date")?;
		// the empty `Fetched` carries no cursor, so a skip leaves the checkpoint where it is
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
	fetched.cursor = Some(today.to_string());
	Ok(fetched)
}

async fn telegram_peer(client: &Client, handle: &str) -> Result<grammers_client::session::types::PeerRef> {
	let peer = client.resolve_username(handle).await?.ok_or_else(|| eyre!("no such telegram username: `{handle}`"))?;
	peer.to_ref().await.map_err(|e| eyre!("{e}"))?.ok_or_else(|| eyre!("`{handle}` resolved but has no usable ref"))
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

	#[test]
	fn cursor_walks_forward() {
		assert_eq!(next_after(&[30, 20, 10], 3), Some(30));
		assert_eq!(next_after(&[20, 10], 3), None);
		assert_eq!(next_after(&[], 3), None);
	}
}
