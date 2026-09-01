use std::{collections::HashMap, convert::Infallible};

use clap::Args;
use color_eyre::eyre::{Context, Result, eyre};
use jiff::{SignedDuration, Timestamp};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use social_networks_utils::utils::btc_price;
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client},
	llm::LlmConfig,
	telegram_dms::TelegramConfig,
	telegram_notifier::TelegramNotifier,
};

const SURFACE: &str = "youtube";
#[derive(Args)]
pub struct YoutubeArgs {}

#[derive(Clone, Debug, Default, MyConfigPrimitives)]
pub struct YoutubeConfig {
	#[primitives(skip)]
	pub channels: HashMap<String, String>,
}

pub struct YoutubeMonitor {
	youtube_config: YoutubeConfig,
	telegram_config: TelegramConfig,
	llm_config: LlmConfig,
}

impl YoutubeMonitor {
	pub fn new(youtube_config: YoutubeConfig, telegram_config: TelegramConfig, llm_config: LlmConfig) -> Self {
		Self {
			youtube_config,
			telegram_config,
			llm_config,
		}
	}
}

impl Client for YoutubeMonitor {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		println!("YouTube: Listening...");
		info!("Monitoring channels: {:?}", self.youtube_config.channels.keys());

		loop {
			match run_youtube_monitor(&self.youtube_config, &self.telegram_config, &self.llm_config).await {
				Err(YoutubeError::Auth(detail)) => return Err(AdapterError::Auth { surface: SURFACE, detail }),
				Err(YoutubeError::Recoverable(e)) => {
					error!("YouTube monitor error: {e:#}");
					error!("Reconnecting in 5 minutes...");
					time::sleep(Duration::from_secs(5 * 60)).await;
				}
			}
		}
	}
}

enum YoutubeError {
	Auth(String),
	Recoverable(color_eyre::eyre::Report),
}

impl<E: Into<color_eyre::eyre::Report>> From<E> for YoutubeError {
	fn from(e: E) -> Self {
		YoutubeError::Recoverable(e.into())
	}
}

async fn ok_or_classify(response: reqwest::Response, op: &str) -> Result<reqwest::Response, YoutubeError> {
	let status = response.status();
	if status.is_success() {
		return Ok(response);
	}
	let body = response.text().await.map_err(color_eyre::eyre::Report::from)?;
	if matches!(status.as_u16(), 401 | 403) {
		return Err(YoutubeError::Auth(format!("{op}: {status}: {body}")));
	}
	Err(YoutubeError::Recoverable(color_eyre::eyre::eyre!("{op}: {status}: {body}")))
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LastUploadedTitles {
	channels: HashMap<String, String>,
}

#[instrument(skip_all)]
async fn run_youtube_monitor(youtube_config: &YoutubeConfig, telegram_config: &TelegramConfig, llm_config: &LlmConfig) -> Result<Infallible, YoutubeError> {
	let client = reqwest::Client::new();
	let telegram = TelegramNotifier::new(telegram_config.clone());

	let state_file = xdg::BaseDirectories::with_prefix("social_networks")
		.place_state_file("youtube_last_uploaded.json")
		.map_err(color_eyre::eyre::Report::from)?;
	let mut last_uploaded: LastUploadedTitles = if state_file.exists() {
		let content = std::fs::read_to_string(&state_file)?;
		serde_json::from_str(&content)?
	} else {
		LastUploadedTitles::default()
	};

	info!("--YouTube-- monitor started");

	//LOOP: daemon - runs until process termination
	loop {
		for (channel_name, channel_id) in &youtube_config.channels {
			match check_channel(&client, channel_id, channel_name, &mut last_uploaded, &telegram, llm_config).await {
				Ok(_) => debug!("Checked channel: {channel_name}"),
				Err(YoutubeError::Auth(detail)) => return Err(YoutubeError::Auth(detail)),
				Err(YoutubeError::Recoverable(e)) => error!("Error checking channel {channel_name}: {e:#}"),
			}
		}

		let state_json = serde_json::to_string(&last_uploaded)?;
		std::fs::write(&state_file, state_json)?;

		time::sleep(Duration::from_secs(60)).await;
	}
}

#[instrument(skip(client, last_uploaded, telegram, llm_config))]
async fn check_channel(
	client: &reqwest::Client,
	channel_id: &str,
	channel_name: &str,
	last_uploaded: &mut LastUploadedTitles,
	telegram: &TelegramNotifier,
	llm_config: &LlmConfig,
) -> Result<(), YoutubeError> {
	let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");

	let response = client.get(&url).send().await.context("Failed to fetch YouTube RSS feed")?;
	let response = ok_or_classify(response, "youtube_rss").await?;

	let xml_content = response.text().await.map_err(color_eyre::eyre::Report::from)?;

	let (video_id, title, published) = parse_youtube_rss(&xml_content)?;

	let now = Timestamp::now();
	let time_since_upload: SignedDuration = now.duration_since(published);

	if time_since_upload < SignedDuration::from_mins(15) {
		if let Some(last_title) = last_uploaded.channels.get(channel_name)
			&& last_title == &title
		{
			return Ok(());
		}

		// nothing is reported or recorded until the notification is out, so a failed cycle is simply retried on the next one
		let sentiment = analyze_sentiment(&title, llm_config).await?;
		telegram.send_youtube_notification(channel_name, &title, &sentiment, &video_id).await?;

		println!("YouTube: [{channel_name}] uploaded: {title}");
		info!("New video from {channel_name}: {title:?}");
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
					let published_dt: Timestamp = dt_str.parse()?;

					#[allow(clippy::unnecessary_unwrap)]
					return Ok((video_id.unwrap(), title.unwrap(), published_dt));
				}
			}
			_ => {}
		}
		buf.clear();
	}

	Err(eyre!("No video entry found in RSS feed"))
}

async fn analyze_sentiment(title: &str, llm_config: &LlmConfig) -> Result<String> {
	let btc_price = btc_price(3).await?;

	let prompt = format!(
		"You receive a title of a youtube video from a crypto channel and current BTC price in case they reference it. \
		You determine if it projects a bullish/bearish/unclear sentiment. Return your choice in one word without nothing else.\n\n\
		BTC price: {btc_price}\n\
		Title of the video: {title}"
	);

	let response = ask_llm::Client::new(llm_config.into()).model(ask_llm::Model::Slow).ask(&prompt).await?;

	let sentiment = response.text.split_whitespace().next().ok_or_else(|| eyre!("LLM returned an empty sentiment for {title:?}"))?;

	Ok(sentiment.to_lowercase())
}
