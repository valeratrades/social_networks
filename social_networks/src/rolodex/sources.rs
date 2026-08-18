//! The read paths `pull` diffs against. Every fetch is scoped to one handle so a handle that has
//! stopped resolving takes only itself down.

use std::collections::BTreeMap;

use color_eyre::eyre::{Result, WrapErr, eyre};
use grammers_client::Client;
use grammers_tl_types as tl;
use jiff::{Timestamp, tz::TimeZone};
use tracing::warn;

/// How far back to reach on the very first pull of a conversation. Without a checkpoint there is no
/// natural stopping point, and a decade of DMs is neither affordable nor interesting.
const INITIAL_MESSAGES: usize = 50;
/// Ceiling per pull once a checkpoint exists. Whatever is left over is picked up by the next run.
const MAX_MESSAGES: usize = 500;
const PAGE: usize = 100;

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

	pub async fn fetch(&self, handle: &str, cursor: Option<&str>) -> Result<Fetched> {
		let channels: Vec<serde_json::Value> = self.get("https://discord.com/api/v10/users/@me/channels").await?;
		let (channel_id, user_id) = channels
			.iter()
			.find_map(|c| {
				let channel_id = c.get("id")?.as_str()?;
				let recipient = c.get("recipients")?.as_array()?.iter().find(|r| r.get("username").and_then(|u| u.as_str()) == Some(handle))?;
				Some((channel_id.to_string(), recipient.get("id")?.as_str()?.to_string()))
			})
			.ok_or_else(|| eyre!("no discord DM channel with `{handle}`"))?;

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
			None => self.messages(&channel_id, None, INITIAL_MESSAGES).await?,
			Some(cursor) => {
				let mut after: u64 = cursor.parse().wrap_err("a discord checkpoint is a snowflake")?;
				let mut raw = Vec::new();
				loop {
					let page = self.messages(&channel_id, Some(after), PAGE).await?;
					let ids: Vec<u64> = page.iter().map(|(id, _)| *id).collect();
					raw.extend(page);
					if raw.len() >= MAX_MESSAGES {
						warn!("discord `{handle}`: stopping at {MAX_MESSAGES} messages, the rest comes on the next pull");
						break;
					}
					match next_after(&ids, PAGE) {
						Some(next) => after = next,
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

	async fn messages(&self, channel_id: &str, after: Option<u64>, limit: usize) -> Result<Vec<(u64, serde_json::Value)>> {
		let mut query = vec![("limit".to_string(), limit.to_string())];
		if let Some(after) = after {
			query.push(("after".to_string(), after.to_string()));
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

pub async fn telegram(client: &Client, handle: &str, cursor: Option<&str>) -> Result<Fetched> {
	let peer = client.resolve_username(handle).await?.ok_or_else(|| eyre!("no such telegram username: `{handle}`"))?;
	let peer_ref = peer.to_ref().await.map_err(|e| eyre!("{e}"))?.ok_or_else(|| eyre!("`{handle}` resolved but has no usable ref"))?;

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

	#[test]
	fn cursor_walks_forward() {
		assert_eq!(next_after(&[30, 20, 10], 3), Some(30));
		assert_eq!(next_after(&[20, 10], 3), None);
		assert_eq!(next_after(&[], 3), None);
	}
}
