use std::{
	collections::BTreeMap,
	time::{SystemTime, UNIX_EPOCH},
};

use clap::Args;
use color_eyre::eyre::{Context, Result, eyre};
use hmac::{Hmac, Mac};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha1::Sha1;

use crate::config::AppConfig;

type HmacSha1 = Hmac<Sha1>;

pub fn main(config: AppConfig, _args: TwitterScheduleArgs) -> Result<()> {
	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async { post_hello_world(&config).await })
}

async fn post_hello_world(config: &AppConfig) -> Result<()> {
	let oauth = config.twitter.oauth.as_ref().ok_or_else(|| eyre!("twitter.oauth config not found"))?;

	println!("Posting 'hello world' from account: {}", oauth.acc_username);

	let tweet_text = "hello world";
	let request = CreateTweetRequest { text: tweet_text.to_string() };

	let response = post_tweet(&oauth.api_key, &oauth.api_key_secret, &oauth.access_token, &oauth.access_token_secret, &request).await?;

	println!("Successfully posted tweet!");
	println!("Tweet ID: {}", response.data.id);
	println!("Tweet text: {}", response.data.text);

	Ok(())
}

async fn post_tweet(api_key: &str, api_key_secret: &str, access_token: &str, access_token_secret: &str, tweet: &CreateTweetRequest) -> Result<CreateTweetResponse> {
	let url = "https://api.twitter.com/2/tweets";
	let method = "POST";

	// Generate OAuth parameters
	let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().to_string();
	let nonce: String = rand::thread_rng().sample_iter(&rand::distributions::Alphanumeric).take(32).map(char::from).collect();

	let mut oauth_params = BTreeMap::new();
	oauth_params.insert("oauth_consumer_key", api_key);
	oauth_params.insert("oauth_nonce", &nonce);
	oauth_params.insert("oauth_signature_method", "HMAC-SHA1");
	oauth_params.insert("oauth_timestamp", &timestamp);
	oauth_params.insert("oauth_token", access_token);
	oauth_params.insert("oauth_version", "1.0");

	// Create parameter string (for signature base string, we only use oauth params for POST with JSON body)
	let param_string = oauth_params
		.iter()
		.map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
		.collect::<Vec<_>>()
		.join("&");

	// Create signature base string
	let signature_base = format!("{}&{}&{}", method, percent_encode(url), percent_encode(&param_string));

	// Create signing key
	let signing_key = format!("{}&{}", percent_encode(api_key_secret), percent_encode(access_token_secret));

	// Generate signature
	let mut mac = HmacSha1::new_from_slice(signing_key.as_bytes()).map_err(|e| eyre!("Failed to create HMAC: {}", e))?;
	mac.update(signature_base.as_bytes());
	let signature = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, mac.finalize().into_bytes());

	// Build Authorization header
	let mut auth_header_params = oauth_params.clone();
	auth_header_params.insert("oauth_signature", &signature);

	let auth_header = format!(
		"OAuth {}",
		auth_header_params.iter().map(|(k, v)| format!(r#"{}="{}""#, k, percent_encode(v))).collect::<Vec<_>>().join(", ")
	);

	// Make the request
	let client = reqwest::Client::new();
	let response = client
		.post(url)
		.header("Authorization", auth_header)
		.header("Content-Type", "application/json")
		.json(tweet)
		.send()
		.await
		.context("Failed to send tweet request")?;

	let status = response.status();
	let response_text = response.text().await.context("Failed to read response body")?;

	if !status.is_success() {
		return Err(eyre!("Twitter API error (status {}): {}", status, response_text));
	}

	let tweet_response: CreateTweetResponse = serde_json::from_str(&response_text).context("Failed to parse tweet response")?;

	Ok(tweet_response)
}

fn percent_encode(s: &str) -> String {
	s.chars()
		.map(|c| match c {
			'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '.' | '_' | '~' => c.to_string(),
			_ => format!("%{:02X}", c as u8),
		})
		.collect()
}

#[derive(Args)]
pub struct TwitterScheduleArgs {}

#[derive(Debug, Serialize)]
struct CreateTweetRequest {
	text: String,
}

#[derive(Debug, Deserialize)]
struct CreateTweetResponse {
	data: TweetData,
}

#[derive(Debug, Deserialize)]
struct TweetData {
	id: String,
	text: String,
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_serialize_tweet_request() {
		let request = CreateTweetRequest { text: "hello world".to_string() };
		let json = serde_json::to_string(&request).unwrap();
		insta::assert_snapshot!(json, @r#"{"text":"hello world"}"#);
	}

	#[test]
	fn test_percent_encode() {
		assert_eq!(percent_encode("hello world"), "hello%20world");
		assert_eq!(percent_encode("Hello-World_123"), "Hello-World_123");
		assert_eq!(percent_encode("test@example.com"), "test%40example.com");
		assert_eq!(percent_encode("~test"), "~test");
	}

	#[test]
	fn test_percent_encode_oauth_signature() {
		// Test case from OAuth spec examples
		let signature = "tnnArxj06cWHq44gCs1OSKk/jLY=";
		let encoded = percent_encode(signature);
		assert_eq!(encoded, "tnnArxj06cWHq44gCs1OSKk%2FjLY%3D");
	}
}
