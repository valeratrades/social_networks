//! Incoming webhooks, a bot's direct messages, and the OAuth a person installs that bot by.

use std::str::FromStr;

use jiff::SignedDuration;
use reqwest::{Client, RequestBuilder, Url};
use serde_json::{Value, json};
use zeroize::Zeroizing;

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
		let got = answer("the webhook", Auth::Address, self.http.get(self.url(at))).await?;
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
		answer("the webhook", Auth::Address, posted(request, message, ping)?).await.map(drop)
	}

	fn url(&self, of: &Webhook) -> String {
		format!("{}/api/webhooks/{}/{}", self.base, of.id, of.token)
	}
}

#[derive(Clone)]
pub struct Bot {
	http: Client,
	base: String,
	token: Zeroizing<String>,
}
impl Bot {
	/// `base`: [`BASE`], or a test's mock.
	pub fn try_new(base: &str, token: Zeroizing<String>) -> Result<Self, Failure> {
		Ok(Self {
			http: client().build()?,
			base: base.trim_end_matches('/').to_owned(),
			token,
		})
	}

	/// The channel of the direct messages with `user_id`, opened when there is none.
	pub async fn dm(&self, user_id: &str) -> Result<String, Failure> {
		let opened = answer("opening a DM", Auth::Ours, self.post("users/@me/channels").json(&json!({"recipient_id": user_id}))).await?;
		opened
			.get("id")
			.and_then(Value::as_str)
			.map(str::to_owned)
			.ok_or_else(|| Failure::Refused("opening a DM: no id".into()))
	}

	pub async fn send(&self, channel_id: &str, message: &Message) -> Result<(), Failure> {
		let request = self.post(&format!("channels/{channel_id}/messages"));
		answer("sending", Auth::Ours, posted(request, message, false)?).await.map(drop)
	}

	fn post(&self, path: &str) -> RequestBuilder {
		self.http
			.post(format!("{}/api/v10/{path}", self.base))
			.header("authorization", format!("Bot {}", self.token.as_str()))
	}
}
impl std::fmt::Debug for Bot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Bot").field("base", &self.base).finish_non_exhaustive()
	}
}

/// A person installing the bot to their account: what it takes for the bot to message them.
#[derive(Clone)]
pub struct OAuth {
	http: Client,
	base: String,
	client_id: String,
	secret: Zeroizing<String>,
	redirect: Url,
}
impl OAuth {
	/// `base`: [`BASE`], or a test's mock.
	pub fn try_new(base: &str, client_id: &str, secret: Zeroizing<String>, redirect: Url) -> Result<Self, Failure> {
		Ok(Self {
			http: client().build()?,
			base: base.trim_end_matches('/').to_owned(),
			client_id: client_id.to_owned(),
			secret,
			redirect,
		})
	}

	pub fn authorize_url(&self, state: &str) -> Url {
		let query = [
			("client_id", self.client_id.as_str()),
			("response_type", "code"),
			("redirect_uri", self.redirect.as_str()),
			("scope", "identify applications.commands"),
			("integration_type", "1"),
			("state", state),
		];
		Url::parse_with_params(&format!("{}/oauth2/authorize", self.base), query).expect("the token is asked for under the same base")
	}

	pub async fn exchange(&self, code: &str) -> Result<User, Failure> {
		let form = [
			("client_id", self.client_id.as_str()),
			("client_secret", self.secret.as_str()),
			("grant_type", "authorization_code"),
			("code", code),
			("redirect_uri", self.redirect.as_str()),
		];
		let granted = answer("the code", Auth::Ours, self.http.post(format!("{}/api/v10/oauth2/token", self.base)).form(&form)).await?;
		let token = granted
			.get("access_token")
			.and_then(Value::as_str)
			.ok_or_else(|| Failure::Refused("the code: no access_token".into()))?;
		let me = answer("the account", Auth::Ours, self.http.get(format!("{}/api/v10/users/@me", self.base)).bearer_auth(token)).await?;
		let field = |k: &str| me.get(k).and_then(Value::as_str).map(str::to_owned);
		Ok(User {
			id: field("id").ok_or_else(|| Failure::Refused("the account: no id".into()))?,
			username: field("username").ok_or_else(|| Failure::Refused("the account: no username".into()))?,
			global_name: field("global_name"),
		})
	}
}
impl std::fmt::Debug for OAuth {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("OAuth").field("base", &self.base).field("client_id", &self.client_id).finish_non_exhaustive()
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct User {
	pub id: String,
	pub username: String,
	pub global_name: Option<String>,
}

/// How long a 429's body says to wait.
pub(crate) fn retry_after(body: &Value) -> Option<SignedDuration> {
	body.get("retry_after")?.as_f64().and_then(|s| SignedDuration::try_from_secs_f64(s).ok()) // a wait that is no number is no wait named
}

/// Whose credential a 401 is about.
enum Auth {
	Address,
	Ours,
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

async fn answer(what: &str, auth: Auth, request: RequestBuilder) -> Result<Value, Failure> {
	let answer = Answer::of(request, ANSWER).await?;
	let said = answer.json();
	if answer.ok() {
		return Ok(said.unwrap_or(Value::Null));
	}
	// without a code it is not Discord's API that answered, but what stands in front of it
	let coded = said.as_ref().is_some_and(|s| s.get("code").is_some_and(Value::is_number));
	Err(match (answer.status, auth) {
		(429, _) => Answer {
			retry_after: said.as_ref().and_then(retry_after).or(answer.retry_after),
			..answer
		}
		.failure(what),
		(401, Auth::Ours) | (403 | 404, _) if !coded || answer.status == 401 => Failure::Transient(answer.why(what)),
		(401, Auth::Address) | (403 | 404, _) => Failure::Gone(answer.why(what)),
		_ => answer.failure(what),
	})
}
