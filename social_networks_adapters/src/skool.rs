use std::{
	collections::{BTreeSet, HashMap},
	convert::Infallible,
};

use clap::Args;
use color_eyre::eyre::{Result, eyre};
use serde::{Deserialize, Serialize};
use social_networks_utils::skool::{Skool, SkoolCredentials};
use tokio::time::{self, Duration};
use tracing::{debug, error, info, instrument};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client},
	telegram_dms::TelegramConfig,
	telegram_notifier::TelegramNotifier,
};

const SURFACE: &str = "skool";
/// The feed is a page of 30, so a group would have to move faster than that between polls to lose a
/// post. Skool publishes no rate limit, and a login is only re-minted on rotation either way.
const POLL: Duration = Duration::from_secs(5 * 60);

#[derive(Args)]
pub struct SkoolArgs {}

#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct SkoolConfig {
	pub email: String,
	pub password: String,
	#[primitives(skip)]
	pub groups: Vec<String>,
}

impl From<&SkoolConfig> for SkoolCredentials {
	fn from(config: &SkoolConfig) -> Self {
		Self {
			email: config.email.clone(),
			password: config.password.clone(),
		}
	}
}

pub struct SkoolWatch {
	skool_config: SkoolConfig,
	telegram_config: TelegramConfig,
}

impl SkoolWatch {
	pub fn new(skool_config: SkoolConfig, telegram_config: TelegramConfig) -> Self {
		Self { skool_config, telegram_config }
	}
}

impl Client for SkoolWatch {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		println!("Skool: Listening...");
		info!("Monitoring groups: {:?}", self.skool_config.groups);

		loop {
			match run_skool_monitor(&self.skool_config, &self.telegram_config).await {
				Err(SkoolError::Auth(detail)) => return Err(AdapterError::Auth { surface: SURFACE, detail }),
				Err(SkoolError::Recoverable(e)) => {
					error!("Skool monitor error: {e:#}");
					error!("Reconnecting in 5 minutes...");
					time::sleep(POLL).await;
				}
			}
		}
	}
}

enum SkoolError {
	Auth(String),
	Recoverable(color_eyre::eyre::Report),
}

impl<E: Into<color_eyre::eyre::Report>> From<E> for SkoolError {
	fn from(e: E) -> Self {
		SkoolError::Recoverable(e.into())
	}
}

/// What was on each group's first page last time round. Comparing against the page rather than
/// against a high-water mark is what keeps a pinned post from re-announcing itself.
#[derive(Debug, Default, Deserialize, Serialize)]
struct Seen {
	groups: HashMap<String, BTreeSet<String>>,
}

#[instrument(skip_all)]
async fn run_skool_monitor(skool_config: &SkoolConfig, telegram_config: &TelegramConfig) -> Result<Infallible, SkoolError> {
	let mut session = Skool::try_new(Some(skool_config.into()))?;
	let telegram = TelegramNotifier::new(telegram_config.clone());

	let state_file = xdg::BaseDirectories::with_prefix("social_networks")
		.place_state_file("skool_seen.json")
		.map_err(color_eyre::eyre::Report::from)?;
	let mut seen: Seen = if state_file.exists() {
		serde_json::from_str(&std::fs::read_to_string(&state_file)?)?
	} else {
		Seen::default()
	};

	info!("--Skool-- monitor started");

	//LOOP: daemon - runs until process termination
	loop {
		for group in &skool_config.groups {
			match check_group(&mut session, group, &mut seen, &telegram).await {
				Ok(()) => debug!("Checked group: {group}"),
				Err(SkoolError::Auth(detail)) => return Err(SkoolError::Auth(detail)),
				Err(SkoolError::Recoverable(e)) => error!("Error checking group {group}: {e:#}"),
			}
		}

		std::fs::write(&state_file, serde_json::to_string(&seen)?)?;
		time::sleep(POLL).await;
	}
}

#[instrument(skip(session, seen, telegram))]
async fn check_group(session: &mut Skool, group: &str, seen: &mut Seen, telegram: &TelegramNotifier) -> Result<(), SkoolError> {
	let payload = session.page(&format!("/{group}")).await?;
	let route = payload.get("page").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool served a page without a route"))?;
	// `page` already re-minted the cookie if it could, so anything but the feed is a dead session or a
	// group we are not in — neither is retriable
	if route != "/[group]" {
		return Err(SkoolError::Auth(format!("`{group}` served {route}")));
	}

	let posts = payload
		.pointer("/props/pageProps/postTrees")
		.and_then(|v| v.as_array())
		.ok_or_else(|| eyre!("`{group}`: no postTrees"))?;
	let mut page = Vec::with_capacity(posts.len());
	for node in posts {
		let post = node.get("post").ok_or_else(|| eyre!("a skool postTree without a post: {node}"))?;
		let id = post.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool post without an id: {post}"))?;
		let title = post.pointer("/metadata/title").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without a title"))?;
		let name = post.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without a slug"))?;
		page.push((id.to_string(), title.to_string(), name.to_string()));
	}

	// the first sight of a group is its whole first page, which is history rather than news
	if let Some(previous) = seen.groups.get(group) {
		for (_, title, name) in page.iter().filter(|(id, ..)| !previous.contains(id)) {
			// nothing is recorded until the notification is out, so a failed cycle is simply retried
			telegram.send_skool_post(group, title, name).await?;
			println!("Skool: [{group}] {title}");
			info!("New post in {group}: {title:?}");
		}
	}
	seen.groups.insert(group.to_string(), page.into_iter().map(|(id, ..)| id).collect());
	Ok(())
}
