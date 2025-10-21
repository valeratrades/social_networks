use std::collections::HashMap;

use clap::Args;
use color_eyre::eyre::{Context, Result};
use jiff::{Timestamp, fmt::strtime};
use serde::{Deserialize, Serialize};
use tokio::time::{self, Duration};
use tracing::{error, info};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier};

#[derive(Args)]
pub struct TwitterArgs {}

#[derive(Debug, Deserialize, Serialize)]
struct TwitterApiUser {
	id: String,
	name: String,
	username: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct TwitterListResponse {
	data: Vec<TwitterApiUser>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Tweet {
	id: String,
	text: String,
}

#[derive(Debug, Deserialize, Serialize)]
struct TweetResponse {
	data: Tweet,
	includes: Option<TweetIncludes>,
}

#[derive(Debug, Deserialize, Serialize)]
struct TweetIncludes {
	polls: Option<Vec<Poll>>,
}

#[derive(Debug, Deserialize, Serialize)]
struct Poll {
	id: String,
	options: Vec<PollOption>,
}

#[derive(Debug, Deserialize, Serialize)]
struct PollOption {
	label: String,
	votes: u32,
}

#[derive(Debug, Deserialize, Serialize)]
struct UserTweetsResponse {
	data: Vec<Tweet>,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct ParsedTweets {
	poll_tweets: Vec<String>,
	maybe_poll_tweets: Vec<String>,
}

pub fn main(config: AppConfig, _args: TwitterArgs) -> Result<()> {
	// Set up tracing with file logging (truncate old logs)
	let log_file = v_utils::xdg_state_file!("twitter.log");
	if log_file.exists() {
		std::fs::remove_file(&log_file)?;
	}
	let file_appender = tracing_appender::rolling::never(log_file.parent().unwrap(), log_file.file_name().unwrap());
	let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

	tracing_subscriber::fmt().with_writer(non_blocking).with_ansi(false).with_max_level(tracing::Level::DEBUG).init();

	println!("Twitter: Listening...");

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_twitter_monitor(&config).await {
				error!("Twitter monitor error: {e}");
				error!("Reconnecting in 5 minutes...");
				time::sleep(Duration::from_secs(5 * 60)).await;
			}
		}
	})
}

async fn run_twitter_monitor(config: &AppConfig) -> Result<()> {
	let client = reqwest::Client::new();
	let telegram = TelegramNotifier::new(config.telegram.clone());

	// Load or create parsed tweets state
	let state_file = v_utils::xdg_state_file!("twitter_parsed.json");
	let mut parsed_state: ParsedTweets = if state_file.exists() {
		let content = std::fs::read_to_string(&state_file)?;
		serde_json::from_str(&content)?
	} else {
		ParsedTweets::default()
	};

	info!("--Twitter-- monitor started");

	// Track last tweet IDs for each user in each list
	let mut user_last_tweets: HashMap<String, String> = HashMap::new();

	loop {
		// Process everytime polls list
		if let Err(e) = process_list(
			&client,
			config,
			&config.twitter.everytime_polls_list,
			&telegram,
			&mut user_last_tweets,
			&mut parsed_state.poll_tweets,
		)
		.await
		{
			error!("Error processing everytime polls list: {e}");
		}

		// Process sometimes polls list
		if let Err(e) = process_list(
			&client,
			config,
			&config.twitter.sometimes_polls_list,
			&telegram,
			&mut user_last_tweets,
			&mut parsed_state.maybe_poll_tweets,
		)
		.await
		{
			error!("Error processing sometimes polls list: {e}");
		}

		// Save state
		let state_json = serde_json::to_string(&parsed_state)?;
		std::fs::write(&state_file, state_json)?;

		let now = Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC);
		info!("Heartbeat. Time: {}", strtime::format("%m/%d/%y-%H:%M", &now).unwrap());

		// Sleep for 60 seconds
		time::sleep(Duration::from_secs(60)).await;
	}
}

async fn process_list(
	client: &reqwest::Client,
	config: &AppConfig,
	list_id: &str,
	telegram: &TelegramNotifier,
	user_last_tweets: &mut HashMap<String, String>,
	parsed_tweets: &mut Vec<String>,
) -> Result<()> {
	// Get list members
	let url = format!("https://api.twitter.com/2/lists/{list_id}/members");
	let response = client
		.get(&url)
		.header("Authorization", format!("Bearer {}", config.twitter.bearer_token))
		.header("Content-Type", "application/json")
		.send()
		.await
		.context("Failed to get list members")?;

	let list_response: TwitterListResponse = response.json().await.context("Failed to parse list response")?;

	// Check each user's latest tweet
	for (user_idx, member) in list_response.data.iter().enumerate() {
		// Ensure parsed_tweets has enough entries
		while parsed_tweets.len() <= user_idx {
			parsed_tweets.push(String::new());
		}

		let parsed_id = parsed_tweets[user_idx].clone();

		if let Err(e) = check_for_updates(client, config, member, &parsed_id, telegram, user_last_tweets, parsed_tweets, user_idx).await {
			error!("Error checking updates for user {}: {}", member.name, e);
		}
	}

	Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn check_for_updates(
	client: &reqwest::Client,
	config: &AppConfig,
	member: &TwitterApiUser,
	parsed_id: &str,
	telegram: &TelegramNotifier,
	_user_last_tweets: &mut HashMap<String, String>,
	parsed_tweets: &mut [String],
	user_idx: usize,
) -> Result<()> {
	// Get user's latest tweets
	let url = format!("https://api.twitter.com/2/users/{}/tweets", member.id);
	let response = client
		.get(&url)
		.header("Authorization", format!("Bearer {}", config.twitter.bearer_token))
		.header("Content-Type", "application/json")
		.send()
		.await
		.context("Failed to get user tweets")?;

	let tweets_response: UserTweetsResponse = response.json().await.context("Failed to parse tweets response")?;

	if tweets_response.data.is_empty() {
		return Ok(());
	}

	let latest_tweet_id = &tweets_response.data[0].id;

	// Check if we've already parsed this tweet
	if latest_tweet_id == parsed_id {
		return Ok(());
	}

	// Get tweet details with poll expansion
	let url = format!("https://api.twitter.com/2/tweets/{latest_tweet_id}");
	let response = client
		.get(&url)
		.query(&[("expansions", "attachments.poll_ids")])
		.header("Authorization", format!("Bearer {}", config.twitter.bearer_token))
		.header("Content-Type", "application/json")
		.send()
		.await
		.context("Failed to get tweet details")?;

	let tweet_response: TweetResponse = response.json().await.context("Failed to parse tweet response")?;

	// Check if this tweet has a poll
	if let Some(includes) = &tweet_response.includes
		&& let Some(polls) = &includes.polls
		&& !polls.is_empty()
	{
		let poll = &polls[0];
		let mut poll_text = format!("Twitter poll from {}:\n", member.name);
		poll_text.push_str(&format!("- {}\n", tweet_response.data.text));
		for option in &poll.options {
			poll_text.push_str(&format!("    ├{}: {}\n", option.label, option.votes));
		}

		println!("{}", poll_text.trim());
		info!("Found poll from {}: {}", member.name, tweet_response.data.text);

		// Send to Telegram
		if let Err(e) = telegram.send_twitter_poll(&member.name, &tweet_response.data.text, &tweet_response.data.id).await {
			error!("Failed to send poll notification: {}", e);
		}
	}

	// Update parsed tweet ID
	parsed_tweets[user_idx] = latest_tweet_id.clone();

	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn test_deserialize_list_response() {
		let json = r#"{
			"data": [
				{
					"id": "1234567890",
					"name": "Test User",
					"username": "testuser"
				}
			]
		}"#;
		let response: TwitterListResponse = serde_json::from_str(json).unwrap();
		insta::assert_debug_snapshot!(response, @r#"
		TwitterListResponse {
		    data: [
		        TwitterApiUser {
		            id: "1234567890",
		            name: "Test User",
		            username: "testuser",
		        },
		    ],
		}
		"#);
	}

	#[test]
	fn test_deserialize_user_tweets_response() {
		let json = r#"{
			"data": [
				{
					"id": "9876543210",
					"text": "This is a test tweet"
				}
			]
		}"#;
		let response: UserTweetsResponse = serde_json::from_str(json).unwrap();
		insta::assert_debug_snapshot!(response, @r#"
		UserTweetsResponse {
		    data: [
		        Tweet {
		            id: "9876543210",
		            text: "This is a test tweet",
		        },
		    ],
		}
		"#);
	}

	#[test]
	fn test_deserialize_tweet_with_poll() {
		let json = r#"{
			"data": {
				"id": "1234567890",
				"text": "What's your favorite language?"
			},
			"includes": {
				"polls": [
					{
						"id": "poll123",
						"options": [
							{
								"label": "Rust",
								"votes": 100
							},
							{
								"label": "Python",
								"votes": 50
							},
							{
								"label": "JavaScript",
								"votes": 75
							}
						]
					}
				]
			}
		}"#;
		let response: TweetResponse = serde_json::from_str(json).unwrap();
		insta::assert_debug_snapshot!(response, @r#"
		TweetResponse {
		    data: Tweet {
		        id: "1234567890",
		        text: "What's your favorite language?",
		    },
		    includes: Some(
		        TweetIncludes {
		            polls: Some(
		                [
		                    Poll {
		                        id: "poll123",
		                        options: [
		                            PollOption {
		                                label: "Rust",
		                                votes: 100,
		                            },
		                            PollOption {
		                                label: "Python",
		                                votes: 50,
		                            },
		                            PollOption {
		                                label: "JavaScript",
		                                votes: 75,
		                            },
		                        ],
		                    },
		                ],
		            ),
		        },
		    ),
		}
		"#);
	}

	#[test]
	fn test_deserialize_tweet_without_poll() {
		let json = r#"{
			"data": {
				"id": "1234567890",
				"text": "Just a regular tweet"
			}
		}"#;
		let response: TweetResponse = serde_json::from_str(json).unwrap();
		insta::assert_debug_snapshot!(response, @r#"
		TweetResponse {
		    data: Tweet {
		        id: "1234567890",
		        text: "Just a regular tweet",
		    },
		    includes: None,
		}
		"#);
	}

	#[test]
	fn test_deserialize_parsed_tweets_state() {
		let json = r#"{
			"poll_tweets": ["123", "456", "789"],
			"maybe_poll_tweets": ["111", "222"]
		}"#;
		let state: ParsedTweets = serde_json::from_str(json).unwrap();
		insta::assert_debug_snapshot!(state, @r#"
		ParsedTweets {
		    poll_tweets: [
		        "123",
		        "456",
		        "789",
		    ],
		    maybe_poll_tweets: [
		        "111",
		        "222",
		    ],
		}
		"#);
	}
}
