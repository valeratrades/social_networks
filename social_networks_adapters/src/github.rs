//! Unauthenticated: everything read here is public, and 60 requests an hour is far more than a
//! hand-run pull or a `recon` sweep spends. A rate-limit 403 surfaces as that handle's failure.
//!
//! A github venue is either an org (`github:valeratrades`) or a repo (`github:owner/name`) — the
//! slug carrying a `/` is what tells them apart, and it is what their URLs already say.

use std::path::Path;

use color_eyre::eyre::{Result, WrapErr, eyre};
use jiff::Timestamp;
use tracing::warn;

use crate::reach::{Author, Item, Kind, Member, Page, Profile, Profiles, Source, Venue, VenueRef, Window};

/// The feed only reaches back 300 events / 90 days no matter how it is paged, so one page is the
/// whole of what a rare read could have recovered anyway.
const PAGE: usize = 100;

#[derive(Default)]
pub struct Github {
	http: reqwest::Client,
}
impl Github {
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

	/// A public event feed, newest-first, stopped by the checkpoint or the date. `who` names the
	/// author when the feed is one person's; a venue feed carries an actor per event.
	async fn feed(&self, url: &str, window: &Window, kind: Kind, who: Option<&str>, label: &str) -> Result<Page> {
		let events: Vec<serde_json::Value> = self.get(url).await?;
		let full_page = events.len() == PAGE;
		let after: Option<u64> = match window {
			Window::Above { after, .. } => after.as_ref().map(|c| c.parse()).transpose().wrap_err("a github checkpoint is an event id")?,
			// a github feed is a snapshot: it has no floor to page down to
			Window::Below { .. } => return Ok(Page { exhausted: true, ..Page::default() }),
		};

		let mut page = Page { exhausted: true, ..Page::default() };
		let mut reached_cursor = false;
		for event in &events {
			let id: u64 = event
				.get("id")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("github event without an id"))?
				.parse()
				.wrap_err("github event ids are numeric")?;
			if after.is_some_and(|c| id <= c) {
				reached_cursor = true;
				break;
			}
			page.newest.get_or_insert_with(|| id.to_string());
			let created = event.get("created_at").and_then(|v| v.as_str()).ok_or_else(|| eyre!("github event {id} without created_at"))?;
			let at: Timestamp = created.parse().wrap_err("github timestamps are RFC3339")?;
			if window.reached(at) {
				break;
			}
			page.oldest = Some(id.to_string());
			let Some((text, permalink)) = describe(event) else {
				continue;
			};
			page.items.push(Item {
				id: id.to_string(),
				source: Source::Github,
				at,
				kind,
				author: Author::Handle(match who {
					Some(who) => who.to_string(),
					None => event
						.pointer("/actor/login")
						.and_then(|v| v.as_str())
						.ok_or_else(|| eyre!("github event {id} without an actor"))?
						.to_string(),
				}),
				text,
				attachments: Vec::new(),
				permalink: Some(permalink),
			});
			if page.items.len() >= window.limit() {
				break;
			}
		}
		if full_page && !reached_cursor {
			warn!("github `{label}`: the whole {PAGE}-event page was new, anything older is past what the feed keeps");
		}
		page.items.reverse();
		Ok(page)
	}
}

impl Profiles for Github {
	async fn profile(&mut self, handle: &str, window: Window) -> Result<Profile> {
		let mut profile = Profile::default();
		let payload: serde_json::Value = self.get(&format!("https://api.github.com/users/{handle}")).await?;
		profile.state("github:bio", payload.get("bio").and_then(|v| v.as_str()));
		profile.state("github:name", payload.get("name").and_then(|v| v.as_str()));
		profile.activity = self
			.feed(
				&format!("https://api.github.com/users/{handle}/events/public?per_page={PAGE}"),
				&window,
				Kind::Activity,
				Some(handle),
				handle,
			)
			.await?;
		Ok(profile)
	}
}

impl Venue for Github {
	/// An anonymous read belongs to no org, so there is nothing for this session to enumerate — a
	/// github venue is named outright.
	async fn venues(&mut self) -> Result<Vec<VenueRef>> {
		Ok(Vec::new())
	}

	async fn members(&mut self, at: &VenueRef) -> Result<Vec<Member>> {
		let url = match at.slug.contains('/') {
			true => format!("https://api.github.com/repos/{}/contributors?per_page={PAGE}", at.slug),
			false => format!("https://api.github.com/orgs/{}/public_members?per_page={PAGE}", at.slug),
		};
		let people: Vec<serde_json::Value> = self.get(&url).await?;
		people
			.iter()
			.map(|person| {
				let handle = person.get("login").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a github member without a login: {person}"))?;
				Ok(Member {
					handle: handle.to_string(),
					display: handle.to_string(),
					// neither listing states when they joined
					joined: None,
				})
			})
			.collect()
	}

	async fn posts(&mut self, at: &VenueRef, window: Window, _assets: &Path) -> Result<Page> {
		let url = match at.slug.contains('/') {
			true => format!("https://api.github.com/repos/{}/events?per_page={PAGE}", at.slug),
			false => format!("https://api.github.com/orgs/{}/events?per_page={PAGE}", at.slug),
		};
		self.feed(&url, &window, Kind::Post, None, &at.slug).await
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
		"IssuesEvent" if payload.get("action").and_then(|v| v.as_str()) == Some("opened") => {
			let title = payload.pointer("/issue/title").and_then(|v| v.as_str()).unwrap_or("");
			let url = payload.pointer("/issue/html_url").and_then(|v| v.as_str()).unwrap_or(&repo_url);
			Some((format!("opened an issue on {repo}: {title}"), url.to_string()))
		}
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
}
