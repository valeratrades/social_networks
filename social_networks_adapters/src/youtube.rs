use std::{collections::HashMap, convert::Infallible};

use clap::Args;
use color_eyre::eyre::{Context, Result, eyre};
use jiff::{SignedDuration, Timestamp};
use quick_xml::{Reader, events::Event};
use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use social_networks_utils::{db::Database, utils::btc_price};
use tokio::time::{self, Duration};
use tracing::{debug, info, instrument, warn};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client, Degradation, Fault, ok_or_classify},
	llm::LlmConfig,
	telegram_dms::TelegramConfig,
	telegram_notifier::TelegramNotifier,
};

#[cfg(feature = "youtube-reads")]
mod reads;
#[cfg(feature = "youtube-reads")]
pub use reads::{Chapter, Cue, Listed, Video, download, listing, uploads, video};

const SURFACE: &str = "youtube";
const STATE_KEY: &str = "youtube_last_uploaded";
/// An upload older than this is not announced, so a channel unread for as long may have one missed.
const ANNOUNCE_WINDOW: SignedDuration = SignedDuration::from_mins(15);
#[derive(Args)]
pub struct YoutubeArgs {}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct YoutubeConfig {
	#[primitives(skip)]
	pub channels: HashMap<String, String>,
	#[primitives(skip)]
	source: UploadSource,
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

/// Where uploads are learned of: the RSS feeds go dark for hours most nights; the Data API spends a daily quota.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum UploadSource {
	Rss,
	Api(DataApi),
}

#[derive(Clone, Debug, MyConfigPrimitives)]
struct DataApi {
	key: String,
}

struct Upload {
	video_id: String,
	title: String,
	published: Timestamp,
}

impl Client for YoutubeMonitor {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		println!("YouTube: Listening...");
		info!("Monitoring channels: {:?}", self.youtube_config.channels.keys());

		loop {
			let Err(fault) = run_youtube_monitor(&self.youtube_config, &self.telegram_config, &self.llm_config).await;
			let e = fault.retry(SURFACE)?;
			warn!("YouTube monitor error: {e}, retrying in 5 minutes");
			time::sleep(Duration::from_secs(5 * 60)).await;
		}
	}
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct LastUploadedTitles {
	channels: HashMap<String, String>,
}

#[instrument(skip_all)]
async fn run_youtube_monitor(youtube_config: &YoutubeConfig, telegram_config: &TelegramConfig, llm_config: &LlmConfig) -> Result<Infallible, Fault> {
	let client = reqwest::Client::new();
	let telegram = TelegramNotifier::new(telegram_config.clone());

	let db = Database::try_new().await?;
	let mut last_uploaded: LastUploadedTitles = db.state(STATE_KEY).await?.unwrap_or_default(); // none yet: nothing announced so far

	let mut degradation = Degradation::default();
	let started = Timestamp::now();
	let mut last_read: HashMap<&str, Timestamp> = youtube_config.channels.keys().map(|name| (name.as_str(), started)).collect(); // never read since start: blind since start

	info!("--YouTube-- monitor started");

	//LOOP: daemon - runs until process termination
	loop {
		for (channel_name, channel_id) in &youtube_config.channels {
			match check_channel(&client, &youtube_config.source, channel_id, channel_name, &mut last_uploaded, &telegram, llm_config).await {
				Ok(_) => {
					debug!("Checked channel: {channel_name}");
					last_read.insert(channel_name.as_str(), Timestamp::now());
				}
				Err(Fault::Transient(e)) => warn!("Error checking channel {channel_name}: {e}"),
				Err(Fault::Degraded(e)) => debug!("Uploads of {channel_name} unavailable: {e}"), // reported by `degradation` when it spans the announce window
				Err(fault) => return Err(fault),
			}
		}

		db.set_state(STATE_KEY, &last_uploaded).await?;

		let now = Timestamp::now();
		let blind: Vec<&str> = last_read
			.iter()
			.filter(|(_, read)| now.duration_since(**read) >= ANNOUNCE_WINDOW)
			.map(|(name, _)| *name)
			.collect();
		let now = (!blind.is_empty()).then(|| format!("no successful read in {ANNOUNCE_WINDOW:#} for {}", blind.join(", ")));
		degradation.observe(&telegram, SURFACE, now).await;
		time::sleep(Duration::from_secs(60)).await;
	}
}

#[instrument(skip(client, source, last_uploaded, telegram, llm_config))]
async fn check_channel(
	client: &reqwest::Client,
	source: &UploadSource,
	channel_id: &str,
	channel_name: &str,
	last_uploaded: &mut LastUploadedTitles,
	telegram: &TelegramNotifier,
	llm_config: &LlmConfig,
) -> Result<(), Fault> {
	let Some(Upload { video_id, title, published }) = newest_upload(client, source, channel_id).await? else {
		return Ok(());
	};

	let now = Timestamp::now();
	let time_since_upload: SignedDuration = now.duration_since(published);

	if time_since_upload < ANNOUNCE_WINDOW {
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

async fn newest_upload(client: &reqwest::Client, source: &UploadSource, channel_id: &str) -> Result<Option<Upload>, Fault> {
	match source {
		UploadSource::Rss => {
			let url = format!("https://www.youtube.com/feeds/videos.xml?channel_id={channel_id}");
			let response = client.get(&url).send().await.context("Failed to fetch YouTube RSS feed").map_err(Fault::transient)?;
			let response = ok_or_classify(response, "youtube_rss", rss_known).await?;
			Ok(Some(parse_youtube_rss(&response.text().await.map_err(Fault::transient)?)?))
		}
		UploadSource::Api(DataApi { key }) => {
			let uploads = format!(
				"UU{}",
				channel_id
					.strip_prefix("UC")
					.ok_or_else(|| Fault::Unhandled(format!("channel id {channel_id:?} is not a `UC…` id")))?
			);
			let response = client
				.get("https://www.googleapis.com/youtube/v3/playlistItems")
				.query(&[("part", "contentDetails,snippet"), ("maxResults", "50"), ("playlistId", &uploads), ("key", key)])
				.send()
				.await
				.context("Failed to fetch the uploads playlist")
				.map_err(Fault::transient)?;
			let response = ok_or_classify(response, "youtube_playlist_items", api_known).await?;
			newest_published(&response.text().await.map_err(Fault::transient)?)
		}
	}
}

/// A page of 50 costs the same quota unit as 1, and lets a premiere scheduled on top not hide the uploads under it.
fn newest_published(page: &str) -> Result<Option<Upload>, Fault> {
	let page: PlaylistItems = serde_json::from_str(page)?;
	Ok(page
		.items
		.into_iter()
		.filter_map(|item| {
			Some(Upload {
				published: item.content_details.video_published_at?, // `snippet.publishedAt` is when it entered the playlist
				video_id: item.content_details.video_id,
				title: item.snippet.title,
			})
		})
		.max_by_key(|upload| upload.published))
}

/// The feeds endpoint answers 404 for hours most nights, any channel.
pub(crate) fn rss_known(status: StatusCode, _: &str) -> Option<fn(String) -> Fault> {
	(status == StatusCode::NOT_FOUND).then_some(Fault::Degraded)
}

/// Google's 403 is overloaded; the reason in the body says which.
pub(crate) fn api_known(_: StatusCode, body: &str) -> Option<fn(String) -> Fault> {
	#[derive(Deserialize)]
	struct Body {
		error: GoogleError,
	}
	#[derive(Deserialize)]
	struct GoogleError {
		#[serde(default)]
		errors: Vec<Reason>,
		#[serde(default)]
		details: Vec<Reason>,
	}
	#[derive(Deserialize)]
	struct Reason {
		reason: Option<String>,
	}
	let body: Body = serde_json::from_str(body).ok()?; // not Google's error body (a proxy's 502 page): the status is all there is
	body.error
		.errors
		.iter()
		.chain(&body.error.details)
		.filter_map(|r| r.reason.as_deref())
		.find_map(|reason| match reason {
			"quotaExceeded" | "rateLimitExceeded" | "RATE_LIMIT_EXCEEDED" => Some(Fault::Degraded as fn(String) -> Fault), // the daily quota resets at midnight Pacific
			"keyInvalid" | "API_KEY_INVALID" | "API_KEY_SERVICE_BLOCKED" | "forbidden" | "accessNotConfigured" | "SERVICE_DISABLED" => Some(Fault::Auth),
			_ => None,
		})
}

#[derive(Deserialize)]
struct PlaylistItems {
	items: Vec<PlaylistItem>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaylistItem {
	snippet: Snippet,
	content_details: ContentDetails,
}
#[derive(Deserialize)]
struct Snippet {
	title: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ContentDetails {
	video_id: String,
	video_published_at: Option<Timestamp>, // absent until the video is public: scheduled premieres, upcoming lives, private
}

fn parse_youtube_rss(xml: &str) -> Result<Upload> {
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
				let tag_name = e.name().as_ref().to_string();
				if tag_name == "entry" {
					in_entry = true;
				}
				current_tag = tag_name;
			}
			Event::Text(e) if in_entry => {
				let text = e.xml10_content().into_owned();
				match current_tag.as_str() {
					"yt:videoId" => video_id = Some(text),
					"title" => title = Some(text),
					"published" => published = Some(text),
					_ => {}
				}
			}
			Event::End(e) => {
				let tag_name = e.name().as_ref().to_string();
				if tag_name == "entry"
					&& video_id.is_some()
					&& title.is_some()
					&& let Some(dt_str) = published
				{
					let published_dt: Timestamp = dt_str.parse()?;

					#[allow(clippy::unnecessary_unwrap)]
					return Ok(Upload {
						video_id: video_id.unwrap(),
						title: title.unwrap(),
						published: published_dt,
					});
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

	let lease = v_utils::memory_lease::Lease::acquire().await?;
	let response = ask_llm::Client::new(llm_config.into()).model(ask_llm::Model::Slow).ask(&prompt).await?;
	drop(lease);

	let sentiment = response.text.split_whitespace().next().ok_or_else(|| eyre!("LLM returned an empty sentiment for {title:?}"))?;

	Ok(sentiment.to_lowercase())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn unpublished_items_are_not_uploads() {
		let item = |id: &str, published: Option<&str>| {
			let published = published.map(|t| format!(r#", "videoPublishedAt": "{t}""#)).unwrap_or_default();
			format!(r#"{{"kind": "youtube#playlistItem", "snippet": {{"publishedAt": "2026-10-17T10:00:00Z", "title": "{id} title"}}, "contentDetails": {{"videoId": "{id}"{published}}}}}"#)
		};
		let page = |items: &[String]| format!(r#"{{"kind": "youtube#playlistItemListResponse", "items": [{}]}}"#, items.join(","));

		let upload = newest_published(&page(&[
			item("premiere", None),
			item("old", Some("2026-10-01T10:00:00Z")),
			item("new", Some("2026-10-17T10:00:00Z")),
		]))
		.unwrap()
		.expect("two items are published");
		assert_eq!(upload.video_id, "new");

		assert!(newest_published(&page(&[item("premiere", None)])).unwrap().is_none());
		assert!(newest_published(&page(&[])).unwrap().is_none());
	}
}
