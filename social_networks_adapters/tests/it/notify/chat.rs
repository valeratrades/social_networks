//! Telegram, discord and slack: what goes out, and what an answer makes of the address.

use std::time::Duration;

use jiff::SignedDuration;
use serde_json::json;
use social_networks_adapters::notify::{
	Failure, Level, Rich, discord, slack,
	telegram::{Bot, Target},
};

use super::{Mock, message, photo};

const TO: Target = Target { chat_id: 7, topic_id: Some(3) };

async fn telegram(status: u16, body: &str) -> Failure {
	let mock = Mock::answering(status, "", body).await;
	let bot = Bot::try_new(&mock.base, "1:t".to_owned().into()).unwrap();
	bot.send(TO, &message("hi", Level::Normal)).await.unwrap_err()
}

#[tokio::test]
async fn telegram_answers_are_classified() {
	let said = |why: &str| json!({"ok": false, "description": why}).to_string();
	assert_eq!(
		telegram(429, r#"{"ok":false,"parameters":{"retry_after":17}}"#).await,
		Failure::RetryAfter(SignedDuration::from_secs(17))
	);
	assert!(matches!(telegram(403, &said("Forbidden: bot was blocked by the user")).await, Failure::Gone(_)));
	assert!(matches!(telegram(400, &said("Bad Request: chat not found")).await, Failure::Gone(_)));
	assert!(matches!(telegram(400, &said("Bad Request: message is too long")).await, Failure::Refused(_)));
	assert!(matches!(telegram(401, &said("Unauthorized")).await, Failure::Transient(_)));
	assert!(matches!(telegram(502, "<html>").await, Failure::Transient(_)));
	assert!(matches!(telegram(200, "<html>").await, Failure::Refused(_)), "not sent again");
}

#[tokio::test]
async fn the_token_is_in_no_failure() {
	let bot = Bot::try_new("http://127.0.0.1:1", "1:secret".to_owned().into()).unwrap();
	let failure = bot.updates(0, Duration::ZERO).await.unwrap_err();
	assert!(matches!(&failure, Failure::Transient(why) if !why.contains("secret")), "{failure:?}");
	assert!(!format!("{bot:?}").contains("secret"));
}

#[tokio::test]
async fn telegram_takes_the_message_cut_and_silent() {
	let mut mock = Mock::answering(200, "", r#"{"ok":true,"result":{"message_id":9}}"#).await;
	let bot = Bot::try_new(&mock.base, "1:t".to_owned().into()).unwrap();
	let mut text = Rich::from("😀".repeat(3000).as_str());
	text.code("tail");
	assert_eq!(bot.send(TO, &message(text, Level::Low)).await.unwrap(), 9);
	let body = mock.got().await.json();
	let sent = body["text"].as_str().unwrap();
	assert!(sent.ends_with('…'));
	assert!(sent.encode_utf16().count() <= 4096);
	assert_eq!(body["entities"], json!([]), "what it marked is cut off");
	assert_eq!((&body["disable_notification"], &body["message_thread_id"]), (&json!(true), &json!(3)));
	assert_eq!(body["link_preview_options"]["is_disabled"], true);
	assert_eq!(body["reply_markup"]["inline_keyboard"][0][0]["url"], "https://panel.example/a?b=(c)");
}

fn hook() -> discord::Webhook {
	"https://discord.com/api/webhooks/123/to-k_en".parse().unwrap()
}

async fn webhook(status: u16, headers: &str, body: &str) -> Failure {
	let mock = Mock::answering(status, headers, body).await;
	let hooks = discord::Webhooks::try_new(&mock.base).unwrap();
	hooks.send(&hook(), &message("hi", Level::Normal), false).await.unwrap_err()
}

#[tokio::test]
async fn discord_answers_are_classified() {
	assert_eq!(
		webhook(429, "", r#"{"retry_after":2.5,"global":false}"#).await,
		Failure::RetryAfter(SignedDuration::from_millis(2500))
	);
	assert!(matches!(webhook(404, "", r#"{"code":10015,"message":"Unknown Webhook"}"#).await, Failure::Gone(_)));
	assert!(matches!(webhook(401, "", r#"{"code":50027,"message":"Invalid Webhook Token"}"#).await, Failure::Gone(_)));
	assert!(
		matches!(webhook(403, "", "error code: 1010").await, Failure::Transient(_)),
		"what stands in front of discord says nothing of the address"
	);
	assert!(matches!(webhook(400, "", r#"{"code":50035}"#).await, Failure::Refused(_)));

	let mock = Mock::answering(401, "", r#"{"code":0,"message":"401: Unauthorized"}"#).await;
	let bot = discord::Bot::try_new(&mock.base, "t".to_owned().into()).unwrap();
	assert!(matches!(bot.dm("5").await, Err(Failure::Transient(_))), "a bot's token is ours");
}

#[tokio::test]
async fn discord_pings_only_when_told_to() {
	let mut mock = Mock::answering(200, "", "{}").await;
	let hooks = discord::Webhooks::try_new(&mock.base).unwrap();
	hooks.send(&hook(), &message("@everyone *".repeat(1000).as_str(), Level::High), false).await.unwrap();
	let quiet = mock.got().await;
	assert!(quiet.head.starts_with("POST /api/webhooks/123/to-k_en?wait=true "), "{}", quiet.head);
	let body = quiet.json();
	assert_eq!((&body["allowed_mentions"], &body["content"]), (&json!({"parse": []}), &json!(null)));
	let description = body["embeds"][0]["description"].as_str().unwrap();
	assert!(description.encode_utf16().count() <= 4096);
	assert!(description.ends_with("…\n\n[Open](https://panel.example/a?b=%28c%29)"), "{description}");

	hooks.send(&hook(), &message("x", Level::High), true).await.unwrap();
	let body = mock.got().await.json();
	assert_eq!((&body["allowed_mentions"], &body["content"]), (&json!({"parse": ["everyone"]}), &json!("@here")));

	let mut pictured = message("x", Level::High);
	pictured.photo = Some(photo());
	hooks.send(&hook(), &pictured, false).await.unwrap();
	let body = String::from_utf8_lossy(&mock.got().await.body).into_owned();
	assert!(body.contains("attachment://a_b.png"), "{body}");
	assert!(body.contains("filename=\"a_b.png\""), "{body}");
	assert!(body.contains("files[0]"), "{body}");
}

#[test]
fn only_a_platforms_own_urls_are_webhooks() {
	for url in [
		"http://discord.com/api/webhooks/1/t",
		"https://discord.com.evil.example/api/webhooks/1/t",
		"https://discord.com@evil.example/api/webhooks/1/t",
		"https://discord.com:8443/api/webhooks/1/t",
		"https://discord.com/api/webhooks/1/t/slack",
		"https://discord.com/api/webhooks/../users/@me",
		"https://discord.com/api/webhooks/x/t",
	] {
		assert!(matches!(url.parse::<discord::Webhook>(), Err(Failure::Refused(_))), "{url}");
	}
	let canary: discord::Webhook = "https://canary.discord.com/api/webhooks/1/t".parse().unwrap();
	assert_eq!(canary.to_string(), "https://discord.com/api/webhooks/1/t");

	for url in [
		"https://hooks.slack.com/workflows/A/B/C",
		"https://evil.example/services/A/B/C",
		"https://hooks.slack.com/services/A/B",
	] {
		assert!(matches!(url.parse::<slack::Webhook>(), Err(Failure::Refused(_))), "{url}");
	}
}

#[tokio::test]
async fn oauth_asks_for_a_user_install() {
	let oauth = discord::OAuth::try_new(discord::BASE, "42", "s".to_owned().into(), "https://panel.example/cb".parse().unwrap()).unwrap();
	assert_eq!(
		oauth.authorize_url("st").as_str(),
		"https://discord.com/oauth2/authorize?client_id=42&response_type=code&redirect_uri=https%3A%2F%2Fpanel.example%2Fcb&scope=identify+applications.commands&integration_type=1&state=st"
	);
}

#[tokio::test]
async fn slack_answers_are_classified_and_its_text_escaped() {
	let at: slack::Webhook = "https://hooks.slack.com/services/T0/B0/x1".parse().unwrap();
	for (status, headers, body, gone) in [(404, "", "no_service", true), (410, "", "channel_is_archived", true), (400, "", "invalid_payload", false)] {
		let mock = Mock::answering(status, headers, body).await;
		let failure = slack::Webhooks::try_new(&mock.base).unwrap().send(&at, &message("x", Level::Low)).await.unwrap_err();
		assert_eq!(matches!(failure, Failure::Gone(_)), gone, "{failure:?}");
	}
	let mock = Mock::answering(429, "retry-after: 30\r\n", "").await;
	let failure = slack::Webhooks::try_new(&mock.base).unwrap().send(&at, &message("x", Level::Low)).await.unwrap_err();
	assert_eq!(failure, Failure::RetryAfter(SignedDuration::from_secs(30)));

	let mut mock = Mock::answering(200, "", "ok").await;
	slack::Webhooks::try_new(&mock.base).unwrap().send(&at, &message("<!channel>", Level::Low)).await.unwrap();
	let request = mock.got().await;
	assert!(request.head.starts_with("POST /services/T0/B0/x1 "));
	assert_eq!(request.json()["text"], "&lt;!channel&gt;");
}
