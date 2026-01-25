use std::collections::HashMap;

use clap::Args;
use color_eyre::eyre::{Context, Result, bail};
use jiff::{SignedDuration, Timestamp};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument};

use crate::{config::AppConfig, telegram_notifier::TelegramNotifier, utils::btc_price};

pub fn main(config: AppConfig, _args: YoutubeArgs) -> Result<()> {
	v_utils::clientside!("youtube");

	println!("YouTube: Listening...");
	info!("Monitoring channels: {:?}", config.youtube.channels.keys());

	let runtime = tokio::runtime::Runtime::new()?;
	runtime.block_on(async {
		loop {
			if let Err(e) = run_youtube_monitor(&config).await {
				error!("YouTube monitor error: {e}");
				error!("Reconnecting in 5 minutes...");
				time::sleep(Duration::from_secs(5 * 60)).await;
			}
		}
	})
}
#[derive(Args)]
pub struct YoutubeArgs {}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LastUploadedTitles {
	channels: HashMap<String, String>,
}

#[instrument(skip(config))]
async fn run_youtube_monitor(config: &AppConfig) -> Result<()> {
	let client = reqwest::Client::new();
	let telegram = TelegramNotifier::new(config.telegram.clone());

	// Load or create last uploaded titles state
	let state_file = v_utils::xdg_state_file!("youtube_last_uploaded.json");
	let mut last_uploaded: LastUploadedTitles = if state_file.exists() {
		let content = std::fs::read_to_string(&state_file)?;
		serde_json::from_str(&content)?
	} else {
		LastUploadedTitles::default()
	};

	info!("--YouTube-- monitor started");

	//LOOP: daemon - runs until process termination
	loop {
		for (channel_name, channel_id) in &config.youtube.channels {
			match check_channel(&client, channel_id, channel_name, &mut last_uploaded, &telegram).await {
				Ok(_) => debug!("Checked channel: {channel_name}"),
				Err(e) => error!("Error checking channel {channel_name}: {e}"),
			}
		}

		// Save state
		let state_json = serde_json::to_string(&last_uploaded)?;
		std::fs::write(&state_file, state_json)?;

		// Sleep for 60 seconds
		time::sleep(Duration::from_secs(60)).await;
	}
}

#[instrument(skip(client, last_uploaded, telegram))]
async fn check_channel(client: &reqwest::Client, channel_id: &str, channel_name: &str, last_uploaded: &mut LastUploadedTitles, telegram: &TelegramNotifier) -> Result<()> {
	let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");

	let response = client.get(&url).send().await.context("Failed to fetch YouTube RSS feed")?;

	let xml_content = response.text().await?;

	let (video_id, title, published) = parse_youtube_rss(&xml_content)?;

	// Check if it's a new video (published within last 15 minutes)
	let now = Timestamp::now();
	let time_since_upload: SignedDuration = now.duration_since(published);

	if time_since_upload < SignedDuration::from_mins(15) {
		// Check if we've already notified about this video
		if let Some(last_title) = last_uploaded.channels.get(channel_name)
			&& last_title == &title
		{
			return Ok(());
		}

		println!("YouTube: [{channel_name}] uploaded: {title}");
		info!("New video from {channel_name}: {title:?}");

		// Get sentiment analysis
		let sentiment = analyze_sentiment(&title).await.unwrap_or_else(|e| {
			error!("Failed to analyze sentiment: {e}");
			"unclear".to_string()
		});

		// Send notification
		if let Err(e) = telegram.send_youtube_notification(channel_name, &title, &sentiment, &video_id).await {
			error!("Failed to send YouTube notification: {e}");
		}

		// Update last uploaded
		last_uploaded.channels.insert(channel_name.to_string(), title.to_string());
	}

	Ok(())
}

fn parse_youtube_rss(xml: &str) -> Result<(String, String, Timestamp)> {
	let mut reader = Reader::from_str(xml);
	reader.config_mut().trim_text(true);

	let mut buf = Vec::new();
	let mut in_entry = false;
	let mut video_id = None;
	let mut title = None;
	let mut published = None;
	let mut current_tag = String::new();

	while let Ok(event) = reader.read_event_into(&mut buf) {
		match event {
			Event::Eof => break,
			Event::Start(e) => {
				let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
				if tag_name == "entry" {
					in_entry = true;
				}
				current_tag = tag_name;
			}
			Event::Text(e) if in_entry => {
				let text = e.escape_ascii().to_string();
				match current_tag.as_str() {
					"yt:videoId" => video_id = Some(text),
					"title" => title = Some(text),
					"published" => published = Some(text),
					_ => {}
				}
			}
			Event::End(e) => {
				let tag_name = String::from_utf8_lossy(e.name().as_ref()).to_string();
				if tag_name == "entry"
					&& video_id.is_some()
					&& title.is_some()
					&& let Some(dt_str) = published
				{
					// YouTube returns ISO 8601 with offset (e.g., "2025-12-31T08:37:07+00:00")
					let published_dt: Timestamp = dt_str.parse()?;

					#[allow(clippy::unnecessary_unwrap)] // actually leads to borrowship issues
					return Ok((video_id.unwrap(), title.unwrap(), published_dt));
				}
			}
			_ => {}
		}
		buf.clear();
	}

	bail!("No video entry found in RSS feed")
}

async fn analyze_sentiment(title: &str) -> Result<String> {
	// Get current BTC price for context
	let btc_price = btc_price(3).await.unwrap_or(0);

	let prompt = format!(
		"You receive a title of a youtube video from a crypto channel and current BTC price in case they reference it. \
		You determine if it projects a bullish/bearish/unclear sentiment. Return your choice in one word without nothing else.\n\n\
		BTC price: {btc_price}\n\
		Title of the video: {title}"
	);

	let response = ask_llm::oneshot(&prompt).await?;

	// Extract first word (bullish/bearish/unclear)
	let sentiment = response.text.split_whitespace().next().unwrap_or("unclear").to_lowercase();

	Ok(sentiment)
}
