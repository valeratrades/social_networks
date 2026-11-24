//! The Bot API.

use std::{sync::Arc, time::Duration};

use jiff::SignedDuration;
use reqwest::{Client, RequestBuilder};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::{
	Failure, Level, Message, Rich,
	http::{ANSWER, Answer, client},
	rich::Entity,
};

pub const BASE: &str = "https://api.telegram.org";
const TEXT: usize = 4096;
const CAPTION: usize = 1024;

#[derive(Clone)]
pub struct Bot {
	http: Client,
	base: String,
	token: Arc<Zeroizing<String>>,
}
impl Bot {
	/// `base`: [`BASE`], or a test's mock.
	pub fn try_new(base: &str, token: Zeroizing<String>) -> Result<Self, Failure> {
		Ok(Self {
			http: client().build()?,
			base: base.trim_end_matches('/').to_owned(),
			token: Arc::new(token),
		})
	}

	/// The id Telegram gave the message. One over Telegram's length is cut.
	pub async fn send(&self, to: Target, message: &Message) -> Result<i64, Failure> {
		let silent = message.level == Level::Low;
		let buttons = !(message.buttons.is_empty() && message.links.is_empty());
		let (method, sent) = match &message.photo {
			None => {
				let mut body = said(&message.text);
				body["chat_id"] = json!(to.chat_id);
				body["disable_notification"] = json!(silent);
				if let Some(topic) = to.topic_id {
					body["message_thread_id"] = json!(topic);
				}
				if buttons {
					body["reply_markup"] = keyboard(message);
				}
				("sendMessage", self.call("sendMessage", &body, None).await?)
			}
			Some(photo) => {
				let caption = message.text.cut(CAPTION);
				let mut form = reqwest::multipart::Form::new()
					.text("chat_id", to.chat_id.to_string())
					.text("caption_entities", json!(caption.entities).to_string())
					.text("caption", caption.text)
					.text("disable_notification", silent.to_string())
					.part("photo", photo.part(photo.name.clone())?);
				if let Some(topic) = to.topic_id {
					form = form.text("message_thread_id", topic.to_string());
				}
				if buttons {
					form = form.text("reply_markup", keyboard(message).to_string());
				}
				("sendPhoto", answer("sendPhoto", self.request("sendPhoto").multipart(form)).await?)
			}
		};
		sent.get("message_id").and_then(Value::as_i64).ok_or_else(|| Failure::Refused(format!("{method}: no message_id")))
	}

	//@codestyle::skip(swappable-args)
	/// Replaces a message's text and buttons (none: the keyboard is removed).
	pub async fn edit(&self, chat_id: i64, message_id: i64, message: &Message) -> Result<(), Failure> {
		let mut body = said(&message.text);
		body["chat_id"] = json!(chat_id);
		body["message_id"] = json!(message_id);
		body["reply_markup"] = keyboard(message); // an empty keyboard is what removes the buttons
		self.call("editMessageText", &body, None).await.map(drop)
	}

	/// Answers a button press, with a short notice the user sees.
	pub async fn answer(&self, callback: &CallbackId, text: &str) -> Result<(), Failure> {
		self.call("answerCallbackQuery", &json!({"callback_query_id": callback.v, "text": text}), None).await.map(drop)
	}

	/// `chat` is `@name` or a numeric id.
	pub async fn chat(&self, chat: &str) -> Result<ChatInfo, Failure> {
		let got = self.call("getChat", &json!({"chat_id": chat}), None).await?;
		let field = |k: &str| got.get(k).and_then(Value::as_str).map(str::to_owned);
		Ok(ChatInfo {
			id: got.get("id").and_then(Value::as_i64).ok_or_else(|| Failure::Refused("getChat: no id".into()))?,
			kind: field("type").ok_or_else(|| Failure::Refused("getChat: no type".into()))?,
			title: field("title"),
			forum: got.get("is_forum").and_then(Value::as_bool) == Some(true),
		})
	}

	//@codestyle::skip(swappable-args)
	pub async fn is_admin(&self, chat_id: i64, user_id: i64) -> Result<bool, Failure> {
		let member = self.call("getChatMember", &json!({"chat_id": chat_id, "user_id": user_id}), None).await?;
		let status = member.get("status").and_then(Value::as_str).ok_or_else(|| Failure::Refused("getChatMember: no status".into()))?;
		Ok(matches!(status, "creator" | "administrator"))
	}

	/// The bot's username.
	pub async fn me(&self) -> Result<String, Failure> {
		let me = self.call("getMe", &json!({}), None).await?;
		me.get("username")
			.and_then(Value::as_str)
			.map(str::to_owned)
			.ok_or_else(|| Failure::Refused("getMe: no username".into()))
	}

	/// The updates from `offset` on, waiting up to `wait` for one: `(update_id, raw)`.
	pub async fn updates(&self, offset: i64, wait: Duration) -> Result<Vec<(i64, Value)>, Failure> {
		let body = json!({"offset": offset, "timeout": wait.as_secs(), "allowed_updates": ["message", "callback_query"]});
		let Value::Array(all) = self.call("getUpdates", &body, Some(wait + Duration::from_secs(10))).await? else {
			return Err(Failure::Refused("getUpdates: not a list".into()));
		};
		Ok(all.into_iter().filter_map(|u| Some((u.get("update_id")?.as_i64()?, u))).collect())
	}

	fn request(&self, method: &str) -> RequestBuilder {
		self.http.post(format!("{}/bot{}/{method}", self.base, self.token.as_str()))
	}

	async fn call(&self, method: &str, body: &Value, timeout: Option<Duration>) -> Result<Value, Failure> {
		let mut request = self.request(method).json(body);
		if let Some(t) = timeout {
			request = request.timeout(t);
		}
		answer(method, request).await
	}
}
impl std::fmt::Debug for Bot {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		f.debug_struct("Bot").field("base", &self.base).finish_non_exhaustive()
	}
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Target {
	pub chat_id: i64,
	/// A forum's topic.
	pub topic_id: Option<i64>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChatInfo {
	pub id: i64,
	/// `private`, `group`, `supergroup` or `channel`.
	pub kind: String,
	pub title: Option<String>,
	pub forum: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Chat {
	pub id: i64,
	/// A one-to-one chat with the bot.
	pub private: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Account {
	pub username: Option<String>,
	pub first_name: Option<String>,
}

/// A button press, by the id Telegram gave it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CallbackId {
	pub v: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Update {
	/// `/start`, with the payload of a `t.me/<bot>?start=<payload>` link when there is one, typed (not
	/// forwarded) by `from`.
	Start {
		chat: Chat,
		payload: Option<String>,
		from: Account,
	},
	Stop {
		chat: Chat,
	},
	/// Any other message.
	Other {
		chat: Chat,
	},
	/// A button pressed under one of the bot's messages.
	Callback {
		id: CallbackId,
		chat_id: i64,
		message_id: i64,
		data: String,
		/// As the user sees it, to extend.
		message: Rich,
	},
}

/// `None` for what is neither a message nor a button press.
pub fn parse_update(raw: &Value) -> Option<Update> {
	let chat_of = |m: &Value| -> Option<Chat> {
		let chat = m.get("chat")?;
		Some(Chat {
			id: chat.get("id")?.as_i64()?,
			private: chat.get("type").and_then(Value::as_str) == Some("private"),
		})
	};
	if let Some(q) = raw.get("callback_query") {
		let message = q.get("message")?;
		// Telegram adds a link of its own over a URL it sees, and a customer's words must not become one when
		// the message is edited
		const FORMATTING: [&str; 9] = ["code", "pre", "bold", "italic", "underline", "strikethrough", "spoiler", "blockquote", "expandable_blockquote"];
		let entities = message
			.get("entities")
			.and_then(Value::as_array)
			.into_iter()
			.flatten()
			.filter_map(|e| serde_json::from_value::<Entity>(e.clone()).ok()) // one not in the Bot API's shape marks nothing
			.filter(|e| FORMATTING.contains(&e.kind.as_str()))
			.collect();
		return Some(Update::Callback {
			id: CallbackId {
				v: q.get("id")?.as_str()?.to_owned(),
			},
			chat_id: chat_of(message)?.id,
			message_id: message.get("message_id")?.as_i64()?,
			data: q.get("data").and_then(Value::as_str).unwrap_or_default().to_owned(), // a game's button carries none
			message: Rich {
				text: message.get("text").and_then(Value::as_str).unwrap_or_default().to_owned(), // a photo's message carries none
				entities,
			},
		});
	}
	let message = raw.get("message")?;
	let chat = chat_of(message)?;
	// a forwarded `/start <token>` is someone else's link, passed on
	if message.get("forward_origin").is_some() {
		return Some(Update::Other { chat });
	}
	let field = |k: &str| message.get("from").and_then(|f| f.get(k)).and_then(Value::as_str).map(str::to_owned);
	let text = message.get("text").and_then(Value::as_str).unwrap_or_default().trim(); // a sticker carries none
	let (command, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
	Some(match command.split('@').next().expect("a split yields at least one piece") {
		"/start" => Update::Start {
			chat,
			payload: Some(rest.trim()).filter(|p| !p.is_empty()).map(str::to_owned),
			from: Account {
				username: field("username"),
				first_name: field("first_name"),
			},
		},
		"/stop" => Update::Stop { chat },
		_ => Update::Other { chat },
	})
}

/// Only a URL the message marks is previewed, never one in a customer's words.
fn said(text: &Rich) -> Value {
	let text = text.cut(TEXT);
	let preview = text.entities.iter().any(|e| e.kind == "url");
	json!({"text": text.text, "entities": text.entities, "link_preview_options": {"is_disabled": !preview}})
}

fn keyboard(message: &Message) -> Value {
	let pressed = message.buttons.iter().map(|b| json!([{"text": b.label, "callback_data": b.data}]));
	let opened = message.links.iter().map(|l| json!([{"text": l.label, "url": l.url.as_str()}]));
	json!({"inline_keyboard": pressed.chain(opened).collect::<Vec<_>>()})
}

async fn answer(method: &str, request: RequestBuilder) -> Result<Value, Failure> {
	let answer = Answer::of(request, ANSWER).await.map_err(|f| match f {
		Failure::Transient(why) => Failure::Transient(format!("{method}: {why}")),
		other => other,
	})?;
	let said = answer.json();
	let field = |k: &str| said.as_ref().and_then(|s| s.get(k));
	if answer.ok() && field("ok") == Some(&json!(true)) {
		return Ok(field("result").cloned().unwrap_or(Value::Null));
	}
	let why = field("description").and_then(Value::as_str).unwrap_or_default(); // an answer that is not the Bot API's is judged by its status
	let full = format!("{method}: {} {why}", answer.status);
	Err(match answer.status {
		429 => {
			let secs = said.as_ref().and_then(|s| s.pointer("/parameters/retry_after")).and_then(Value::as_i64);
			Answer {
				retry_after: secs.map(SignedDuration::from_secs).or(answer.retry_after),
				..answer
			}
			.failure(method)
		}
		403 => Failure::Gone(full),
		400 if why.contains("chat not found") || why.contains("user is deactivated") => Failure::Gone(full),
		// the token is in the path: both say it is not one
		401 | 404 => Failure::Transient(full),
		500..=599 => Failure::Transient(full),
		_ => Failure::Refused(full),
	})
}
