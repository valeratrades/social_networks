//! Incoming webhooks.

use std::str::FromStr;

use jiff::SignedDuration;
use reqwest::{Client, RequestBuilder, Url};
use serde_json::{Value, json};

use super::{
	Failure, Level, Message,
	http::{ANSWER, Answer, client},
	rich::{Rich, discord_url, escape_discord},
};

pub const BASE: &str = "https://discord.com";
const TITLE: usize = 256;
const DESCRIPTION: usize = 4096;
const SUPPRESS_NOTIFICATIONS: u32 = 1 << 12;

/// A webhook's address; posting to it is all it takes, so it is a secret.
#[derive(Clone, Eq, PartialEq)]
pub struct Webhook {
	id: String,
	token: String,
}
impl FromStr for Webhook {
	type Err = Failure;

	fn from_str(s: &str) -> Result<Self, Failure> {
		let refuse = || Failure::Refused("not a Discord webhook's URL (https://discord.com/api/webhooks/…)".to_owned());
		let url = Url::parse(s.trim()).map_err(|_| refuse())?;
		let host = matches!(url.host_str(), Some("discord.com" | "discordapp.com" | "ptb.discord.com" | "canary.discord.com"));
		let bare = url.scheme() == "https" && url.port().is_none() && url.username().is_empty() && url.password().is_none() && url.query().is_none() && url.fragment().is_none();
		match url.path().split('/').collect::<Vec<_>>()[..] {
			["", "api", "webhooks", id, token]
				if host
					&& bare && !id.is_empty()
					&& id.bytes().all(|b| b.is_ascii_digit())
					&& !token.is_empty()
					&& token.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)) =>
				Ok(Self {
					id: id.to_owned(),
					token: token.to_owned(),
				}),
			_ => Err(refuse()),
		}
	}
}
impl std::fmt::Display for Webhook {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{BASE}/api/webhooks/{}/{}", self.id, self.token)
	}
}
impl std::fmt::Debug for Webhook {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Webhook").field("id", &self.id).finish_non_exhaustive()
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Look {
	pub name: Option<String>,
	pub channel_id: String,
	pub guild_id: Option<String>,
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

	pub async fn look(&self, at: &Webhook) -> Result<Look, Failure> {
		let got = answer("the webhook", self.http.get(self.url(at))).await?;
		let field = |k: &str| got.get(k).and_then(Value::as_str).map(str::to_owned);
		Ok(Look {
			name: field("name"),
			channel_id: field("channel_id").ok_or_else(|| Failure::Refused("the webhook: no channel_id".into()))?,
			guild_id: field("guild_id"),
		})
	}

	/// `ping`: with an `@here`.
	pub async fn send(&self, to: &Webhook, message: &Message, ping: bool) -> Result<(), Failure> {
		let request = self.http.post(self.url(to)).query(&[("wait", "true")]);
		answer("the webhook", posted(request, message, ping)?).await.map(drop)
	}

	fn url(&self, of: &Webhook) -> String {
		format!("{}/api/webhooks/{}/{}", self.base, of.id, of.token)
	}
}

/// How long a 429's body says to wait.
pub(crate) fn retry_after(body: &Value) -> Option<SignedDuration> {
	body.get("retry_after")?.as_f64().and_then(|s| SignedDuration::try_from_secs_f64(s).ok()) // a wait that is no number is no wait named
}

fn posted(request: RequestBuilder, message: &Message, ping: bool) -> Result<RequestBuilder, Failure> {
	let links: Vec<String> = message.links.iter().map(|l| format!("[{}]({})", escape_discord(&l.label), discord_url(&l.url))).collect();
	let links = match links.is_empty() {
		true => String::new(),
		false => format!("\n\n{}", links.join(" · ")),
	};
	let room = DESCRIPTION
		.checked_sub(links.encode_utf16().count())
		.filter(|room| *room > 0)
		.ok_or_else(|| Failure::Refused("the links alone are over Discord's length".into()))?;
	let mut embed = json!({
		"description": message.text.fit(room, Rich::discord) + &links,
		"color": match message.level {
			Level::Low => 0x95a5a6,
			Level::Normal => 0x5865f2,
			Level::High => 0xed4245,
		},
	});
	if let Some(title) = &message.title {
		embed["title"] = json!(Rich::from(title.as_str()).fit(TITLE, Rich::discord));
	}
	let mut body = json!({
		"allowed_mentions": {"parse": if ping { vec!["everyone"] } else { vec![] }},
		"flags": if message.level == Level::Low { SUPPRESS_NOTIFICATIONS } else { 0 },
	});
	if ping {
		body["content"] = json!("@here");
	}
	let Some(photo) = &message.photo else {
		body["embeds"] = json!([embed]);
		return Ok(request.json(&body));
	};
	// the name an embed refers to a file by takes no other characters
	let name: String = photo.name.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '_' }).collect();
	embed["image"] = json!({"url": format!("attachment://{name}")});
	body["embeds"] = json!([embed]);
	let form = reqwest::multipart::Form::new().text("payload_json", body.to_string()).part("files[0]", photo.part(name)?);
	Ok(request.multipart(form))
}

async fn answer(what: &str, request: RequestBuilder) -> Result<Value, Failure> {
	let answer = Answer::of(request, ANSWER).await?;
	let said = answer.json();
	if answer.ok() {
		return Ok(said.unwrap_or(Value::Null));
	}
	// without a code it is not Discord's API that answered, but what stands in front of it
	let coded = said.as_ref().is_some_and(|s| s.get("code").is_some_and(Value::is_number));
	Err(match answer.status {
		429 => Answer {
			retry_after: said.as_ref().and_then(retry_after).or(answer.retry_after),
			..answer
		}
		.failure(what),
		403 | 404 if !coded => Failure::Transient(answer.why(what)),
		401 | 403 | 404 => Failure::Gone(answer.why(what)),
		_ => answer.failure(what),
	})
}
