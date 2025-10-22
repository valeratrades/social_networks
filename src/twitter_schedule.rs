use std::{
	collections::{BTreeMap, HashMap},
	time::{SystemTime, UNIX_EPOCH},
};

use clap::Args;
use color_eyre::eyre::{Context, Result, bail, eyre};
use hmac::{Hmac, Mac};
use jiff::{Timestamp, fmt::strtime};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha1::Sha1;
use tokio::time;
use tracing::{error, info, instrument};

use crate::{
	config::AppConfig,
	utils::{btc_price, format_num_with_thousands},
};

type HmacSha1 = Hmac<Sha1>;

pub fn main(config: AppConfig, _args: TwitterScheduleArgs) -> Result<()> {
	// Set up tracing with file logging (truncate old logs)
	let log_file = v_utils::xdg_state_file!("twitter_schedule.log");
	if log_file.exists() {
		std::fs::remove_file(&log_file)?;
	}
	let file_appender = tracing_appender::rolling::never(log_file.parent().unwrap(), log_file.file_name().unwrap());
	let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

	tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_max_level(tracing::Level::DEBUG).init();

	println!("Twitter Schedule: Starting scheduled poll posting...");

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async { schedule_sentiment_poll(&config).await })
}

/// Runs a scheduling loop that posts sentiment polls at regular intervals
#[instrument]
async fn schedule_sentiment_poll(config: &AppConfig) -> Result<()> {
	println!("Twitter Schedule: Scheduler initialized");

	let poll_config = config.twitter.poll.as_ref().ok_or_else(|| eyre!("twitter.poll config not found"))?;

	// Get the schedule interval
	let schedule_duration = poll_config.schedule_every.duration();
	info!("Schedule interval: {:?}", schedule_duration);
	println!("Schedule interval: {:?}", schedule_duration);

	loop {
		let now = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
		let time_str = strtime::format("%Y-%m-%d %H:%M:%S", &now).unwrap();

		info!("Starting poll posting cycle at {}", time_str);
		println!("\n[{}] Starting poll posting cycle", time_str);

		// Post the poll
		match post_poll(config).await {
			Ok(()) => {
				info!("Poll posted successfully");
				println!("✓ Poll posted successfully");
			}
			Err(e) => {
				error!("Failed to post poll: {:?}", e);
				println!("✗ Failed to post poll: {e}.\nWill retry the next posting cycle");
			}
		}

		let next_time = Timestamp::now()
			.to_zoned(jiff::tz::TimeZone::UTC)
			.checked_add(jiff::Span::try_from(schedule_duration).unwrap())
			.unwrap();
		let next_time_str = strtime::format("%Y-%m-%d %H:%M:%S", &next_time).unwrap();

		info!("Next poll scheduled for: {}", next_time_str);
		println!("Next poll scheduled for: {}", next_time_str);
		println!("Sleeping for {:?}...", schedule_duration);

		// Sleep until next cycle
		time::sleep(schedule_duration).await;
	}
}

#[instrument]
async fn post_poll(config: &AppConfig) -> Result<()> {
	info!("post_poll: Starting");

	let oauth = config.twitter.oauth.as_ref().ok_or_else(|| eyre!("twitter.oauth config not found"))?;
	let poll_config = config.twitter.poll.as_ref().ok_or_else(|| eyre!("twitter.poll config not found"))?;

	info!("post_poll: Using account {}", oauth.acc_username);
	println!("Posting poll from account: {}", oauth.acc_username);

	// Parse poll text and extract options with lazy variable resolution
	info!("post_poll: Parsing poll text and resolving variables");
	let (tweet_text, poll_options) = parse_poll_text_async(&poll_config.text).await?;
	info!("post_poll: Parsed tweet text: {}", tweet_text);
	info!("post_poll: Poll options: {:?}", poll_options);

	let duration_minutes = poll_config.duration_hours * 60;
	info!("post_poll: Poll duration: {} minutes", duration_minutes);

	let request = CreateTweetRequest {
		text: tweet_text.clone(),
		poll: Some(PollOptions {
			duration_minutes,
			options: poll_options.clone(),
		}),
	};

	info!("post_poll: Sending request to Twitter API");
	let response = post_tweet(&oauth.api_key, &oauth.api_key_secret, &oauth.access_token, &oauth.access_token_secret, &request).await?;

	info!("post_poll: Successfully posted poll with ID {}", response.data.id);
	println!("Successfully posted poll!");
	println!("Tweet ID: {}", response.data.id);
	println!("Tweet text: {}", response.data.text);

	Ok(())
}

#[instrument]
async fn post_tweet(api_key: &str, api_key_secret: &str, access_token: &str, access_token_secret: &str, tweet: &CreateTweetRequest) -> Result<CreateTweetResponse> {
	info!("post_tweet: Preparing OAuth request");
	let url = "https://api.twitter.com/2/tweets";
	let method = "POST";

	// Generate OAuth parameters
	let timestamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs().to_string();
	let nonce: String = rand::thread_rng().sample_iter(&rand::distributions::Alphanumeric).take(32).map(char::from).collect();
	info!("post_tweet: Generated OAuth nonce and timestamp");

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
	info!("post_tweet: Making HTTP request to Twitter API");
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
	info!("post_tweet: Received response with status {}", status);
	let response_text = response.text().await.context("Failed to read response body")?;

	if !status.is_success() {
		error!("post_tweet: Twitter API error (status {}): {}", status, response_text);
		return Err(eyre!("Twitter API error (status {}): {}", status, response_text));
	}

	info!("post_tweet: Parsing response JSON");
	let tweet_response: CreateTweetResponse = serde_json::from_str(&response_text).context("Failed to parse tweet response")?;

	Ok(tweet_response)
}

/// Variables provider with lazy async evaluation
#[derive(Debug)]
struct VariableProvider;
impl VariableProvider {
	async fn btc_price() -> Result<String> {
		let price = btc_price().await?;
		let rounded_to_100 = ((price + 50) / 100) * 100;
		let s = format_num_with_thousands(rounded_to_100, ",");

		Ok(s)
	}

	async fn date() -> Result<String> {
		let now = Timestamp::now();
		let zoned = now.to_zoned(jiff::tz::TimeZone::UTC);
		let date_str = zoned.to_string();
		Ok(date_str)
	}

	#[instrument]
	async fn resolve(&self, variable_name: &str) -> Result<String> {
		match variable_name {
			"btc_price" => Self::btc_price().await,
			"date" => Self::date().await,
			_ => Err(eyre!("Unknown variable: {variable_name}")),
		}
	}
}

/// Extract variable names from text (finds all ${var_name} patterns)
fn extract_variable_names(text: &str) -> Vec<String> {
	let mut variables = Vec::new();
	let mut chars = text.chars().peekable();

	while let Some(ch) = chars.next() {
		if ch == '$' && chars.peek() == Some(&'{') {
			chars.next(); // consume '{'
			let mut var_name = String::new();
			while let Some(&c) = chars.peek() {
				if c == '}' {
					chars.next(); // consume '}'
					variables.push(var_name);
					break;
				} else {
					var_name.push(c);
					chars.next();
				}
			}
		}
	}

	variables
}

#[instrument]
async fn parse_poll_text_async(text: &str) -> Result<(String, Vec<String>)> {
	// First, extract variable names needed
	let variable_names = extract_variable_names(text);
	info!(?variable_names);

	// Resolve only the variables we need
	let provider = VariableProvider;
	let mut variables = HashMap::new();

	for var_name in variable_names {
		let value = provider.resolve(&var_name).await?;
		variables.insert(var_name, value);
	}

	// Now parse the poll text
	let result = parse_poll_text(text, &variables)?;
	Ok(result)
}

fn parse_poll_text(text: &str, variables: &HashMap<String, String>) -> Result<(String, Vec<String>)> {
	let mut tweet_lines = Vec::new();
	let mut poll_options = Vec::new();

	for line in text.lines() {
		let trimmed = line.trim();
		if let Some(option_text) = trimmed.strip_prefix("- [ ] ") {
			// This is a poll option
			poll_options.push(option_text.to_string());
		} else {
			// This is part of the tweet text (including empty lines)
			tweet_lines.push(line);
		}
	}

	// Join tweet lines and perform variable substitution
	let mut tweet_text = tweet_lines.join("\n");

	// Trim trailing/leading whitespace from the entire tweet text
	tweet_text = tweet_text.trim().to_string();

	// Substitute variables
	for (key, value) in variables {
		let placeholder = format!("${{{}}}", key);
		tweet_text = tweet_text.replace(&placeholder, value);
	}

	if poll_options.is_empty() {
		bail!("No poll options found in text. Use '- [ ] option' format");
	}

	if poll_options.len() > 4 {
		bail!("Twitter polls support maximum 4 options, found {}", poll_options.len());
	}

	Ok((tweet_text, poll_options))
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
	#[serde(skip_serializing_if = "Option::is_none")]
	poll: Option<PollOptions>,
}

#[derive(Debug, Serialize)]
struct PollOptions {
	duration_minutes: u32,
	options: Vec<String>,
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
		let request = CreateTweetRequest {
			text: "hello world".to_string(),
			poll: None,
		};
		let json = serde_json::to_string(&request).unwrap();
		insta::assert_snapshot!(json, @r#"{"text":"hello world"}"#);
	}

	#[test]
	fn test_serialize_tweet_with_poll() {
		let request = CreateTweetRequest {
			text: "rust go brrr?".to_string(),
			poll: Some(PollOptions {
				duration_minutes: 1440,
				options: vec!["yes".to_string(), "yes".to_string()],
			}),
		};
		let json = serde_json::to_string(&request).unwrap();
		insta::assert_snapshot!(json, @r#"{"text":"rust go brrr?","poll":{"duration_minutes":1440,"options":["yes","yes"]}}"#);
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

	#[test]
	fn test_extract_variable_names() {
		let text = "Price: ${btc_price}, Date: ${date}";
		let vars = extract_variable_names(text);
		assert_eq!(vars, vec!["btc_price", "date"]);
	}

	#[test]
	fn test_extract_variable_names_empty() {
		let text = "No variables here";
		let vars = extract_variable_names(text);
		assert!(vars.is_empty());
	}

	#[test]
	fn test_parse_poll_text_basic() {
		let text = r#"btc up or down?
- [ ] up
- [ ] down"#;
		let variables = HashMap::new();
		let (tweet_text, options) = parse_poll_text(text, &variables).unwrap();
		assert_eq!(tweet_text, "btc up or down?");
		assert_eq!(options, vec!["up", "down"]);
	}

	#[test]
	fn test_parse_poll_text_with_variable() {
		let text = r#"btc up or down?

for ref, current price: ${btc_price}
- [ ] up
- [ ] down
- [ ] crab
- [ ] see results"#;
		let mut variables = HashMap::new();
		variables.insert("btc_price".to_string(), "1234".to_string());
		let (tweet_text, options) = parse_poll_text(text, &variables).unwrap();
		assert_eq!(tweet_text, "btc up or down?\n\nfor ref, current price: 1234");
		assert_eq!(options, vec!["up", "down", "crab", "see results"]);
	}

	#[test]
	fn test_parse_poll_text_multiple_variables() {
		let text = r#"${coin} price: ${price}
- [ ] buy
- [ ] sell"#;
		let mut variables = HashMap::new();
		variables.insert("coin".to_string(), "BTC".to_string());
		variables.insert("price".to_string(), "$50000".to_string());
		let (tweet_text, options) = parse_poll_text(text, &variables).unwrap();
		assert_eq!(tweet_text, "BTC price: $50000");
		assert_eq!(options, vec!["buy", "sell"]);
	}

	#[test]
	fn test_parse_poll_text_no_options() {
		let text = "just text, no options";
		let variables = HashMap::new();
		let result = parse_poll_text(text, &variables);
		assert!(result.is_err());
		assert!(result.unwrap_err().to_string().contains("No poll options found"));
	}

	#[test]
	fn test_parse_poll_text_too_many_options() {
		let text = r#"pick one
- [ ] option1
- [ ] option2
- [ ] option3
- [ ] option4
- [ ] option5"#;
		let variables = HashMap::new();
		let result = parse_poll_text(text, &variables);
		assert!(result.is_err());
		assert!(result.unwrap_err().to_string().contains("maximum 4 options"));
	}

	#[tokio::test]
	async fn test_parse_poll_text_async_with_btc_price() {
		let text = r#"BTC: ${btc_price}
- [ ] up
- [ ] down"#;
		let (tweet_text, options) = parse_poll_text_async(text).await.unwrap();
		assert_eq!(tweet_text, "BTC: 1234");
		assert_eq!(options, vec!["up", "down"]);
	}

	#[tokio::test]
	async fn test_parse_poll_text_async_with_date() {
		let text = r#"Today: ${date}
- [ ] yes
- [ ] no"#;
		let (tweet_text, options) = parse_poll_text_async(text).await.unwrap();
		assert!(tweet_text.starts_with("Today: "));
		assert!(tweet_text.contains("UTC"));
		assert_eq!(options, vec!["yes", "no"]);
	}

	#[tokio::test]
	async fn test_parse_poll_text_async_no_variables() {
		let text = r#"Simple poll
- [ ] option1
- [ ] option2"#;
		let (tweet_text, options) = parse_poll_text_async(text).await.unwrap();
		assert_eq!(tweet_text, "Simple poll");
		assert_eq!(options, vec!["option1", "option2"]);
	}
}
