//! Incoming webhooks.

use std::str::FromStr;

use reqwest::{Client, Url};
use serde_json::json;

use super::{
	Failure, Message,
	http::{ANSWER, Answer, client},
};

pub const BASE: &str = "https://hooks.slack.com";
const HEADER: usize = 150;
const TEXT: usize = 3000;

/// A webhook's address; posting to it is all it takes, so it is a secret.
#[derive(Clone, Eq, PartialEq)]
pub struct Webhook {
	path: String,
}
impl FromStr for Webhook {
	type Err = Failure;

	fn from_str(s: &str) -> Result<Self, Failure> {
		let refuse = || Failure::Refused(format!("not a Slack webhook's URL ({BASE}/services/…)"));
		let url = Url::parse(s.trim()).map_err(|_| refuse())?;
		let bare = url.scheme() == "https" && url.port().is_none() && url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none();
		match url.path().split('/').collect::<Vec<_>>()[..] {
			["", "services", ref ids @ ..]
				if bare && url.host_str() == Some("hooks.slack.com") && ids.len() == 3 && ids.iter().all(|s| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric())) =>
				Ok(Self { path: url.path().to_owned() }),
			_ => Err(refuse()),
		}
	}
}
impl std::fmt::Display for Webhook {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{BASE}{}", self.path)
	}
}
impl std::fmt::Debug for Webhook {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Webhook").finish_non_exhaustive()
	}
}

#[derive(Clone, Debug)]
pub struct Webhooks {
	http: Client,
	base: String,
}
impl Webhooks {
	/// `base`: [`BASE`], or a test's mock.
	pub fn try_new(base: &str) -> Result<Self, Failure> {
		Ok(Self {
			http: client().build()?,
			base: base.trim_end_matches('/').to_owned(),
		})
	}

	pub async fn send(&self, to: &Webhook, message: &Message) -> Result<(), Failure> {
		let text = message.text.cut(TEXT);
		let mut blocks = Vec::new();
		if let Some(title) = &message.title {
			blocks.push(json!({"type": "header", "text": {"type": "plain_text", "text": super::Rich::from(title.as_str()).cut(HEADER).text, "emoji": false}}));
		}
		let mut elements = text.slack();
		if !message.links.is_empty() {
			let links = message.links.iter().map(|l| json!({"type": "link", "url": l.url.as_str(), "text": l.label}));
			let spaced: Vec<_> = links.flat_map(|l| [json!({"type": "text", "text": "\n"}), l]).collect();
			elements.push(json!({"type": "rich_text_section", "elements": spaced}));
		}
		if !elements.is_empty() {
			blocks.push(json!({"type": "rich_text", "elements": elements}));
		}
		// what a notification shows in the blocks' place; mrkdwn even there
		let summary = message.title.as_deref().unwrap_or(text.plain()).replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
		let body = json!({"text": summary, "mrkdwn": false, "unfurl_links": false, "unfurl_media": false, "blocks": blocks});
		let answer = Answer::of(self.http.post(format!("{}{}", self.base, to.path)).json(&body), ANSWER).await?;
		let said = String::from_utf8_lossy(&answer.body);
		let gone = [
			"action_prohibited",
			"channel_is_archived",
			"channel_not_found",
			"invalid_token",
			"no_active_hooks",
			"no_service",
			"no_service_id",
			"no_team",
			"team_disabled",
			"user_not_found",
		]
		.contains(&said.trim());
		match answer.status {
			_ if answer.ok() => Ok(()),
			_ if gone => Err(Failure::Gone(answer.why("the webhook"))),
			403 | 404 | 410 => Err(Failure::Gone(answer.why("the webhook"))),
			_ => Err(answer.failure("the webhook")),
		}
	}
}
