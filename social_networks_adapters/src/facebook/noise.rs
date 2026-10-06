//! Browsing that is not the work: the feed skimmed and read, somebody's profile clicked into from it,
//! a search. Links are clicked and the search field typed into, and nothing else; see
//! `docs/facebook/noise.md`.

use std::time::Duration;

use color_eyre::eyre::{Result, ensure};
use tokio::time::Instant;
use tracing::info;

use super::{FEED, Facebook, Session};
use crate::{
	behaviour::{Action, log_normal},
	reach::Browsing,
};

const SEARCH: &str = r#"input[aria-label="Search Facebook"]"#;
/// a feed post's own text, the "See more" under it included
const READABLE: &str = r#"[aria-posinset] :is([data-ad-preview="message"], [data-ad-comet-preview="message"])"#;
const LINK: &str = r#"[aria-posinset] a[href]:not([role="button"])"#;
const QUERIES: &[&str] = &[
	"météo lyon",
	"marché croix rousse",
	"recette gratin dauphinois",
	"brocante lyon",
	"fête des lumières",
	"horaires piscine",
	"restaurant vieux lyon",
	"concert lyon",
	"jardinage balcon",
	"recette tarte aux pommes",
	"vélo'v",
	"parc de la tête d'or",
];
/// Single path segments that are a place on facebook rather than somebody.
const RESERVED: &[&str] = &[
	"photo", "photos", "posts", "groups", "watch", "reel", "reels", "events", "marketplace", "stories", "hashtag", "search", "gaming", "friends", "messages", "notifications", "bookmarks",
	"saved", "memories", "settings", "help", "privacy", "policies", "ads", "login", "me", "pages", "sharer",
];

impl Browsing for Facebook<'_, '_> {
	async fn noise(&mut self, span: Duration) -> Result<()> {
		ensure!(self.session != Session::Attached, "the user's own facebook is never browsed for them");
		self.conversation = None;
		let walk = async {
			loop {
				if let Err(e) = self.step().await {
					return e;
				}
			}
		};
		// cut wherever it is at the deadline: nothing a step leaves outlives the next load by URL
		match tokio::time::timeout_at(Instant::now() + span, walk).await {
			Ok(e) => Err(e.wrap_err("browsing for noise")),
			Err(_) => Ok(()),
		}
	}
}

impl Facebook<'_, '_> {
	async fn step(&mut self) -> Result<()> {
		match rand::random_range(0..20) {
			0..12 => self.feed().await?,
			12..17 => self.someone().await?,
			_ => self.search().await?,
		}
		let open: bool = self
			.tab
			.see(
				r#"() => [...document.querySelectorAll('[role="dialog"]')].some(d => { const r = d.getBoundingClientRect(); return r.width > 0 && r.height > 0 })"#,
				(),
			)
			.await?;
		if open {
			info!("noise: a dialog is open on {}; back to the feed", self.href().await?);
			self.home().await?;
		}
		Ok(())
	}

	async fn feed(&mut self) -> Result<()> {
		if self.href().await? != FEED {
			self.home().await?;
		}
		info!("noise: the feed");
		self.browse(rand::random_range(3..=12)).await
	}

	/// A person or a page linked from a post on screen; the feed when none is.
	async fn someone(&mut self) -> Result<()> {
		let hrefs: Vec<String> = self
			.tab
			.see(
				r#"sel => [...document.querySelectorAll(sel)].filter(a => { const r = a.getBoundingClientRect(); return r.width > 0 && r.top >= 0 && r.bottom <= innerHeight }).map(a => a.getAttribute('href'))"#,
				LINK,
			)
			.await?;
		let people: Vec<String> = hrefs.into_iter().filter(|h| somebody(h)).collect();
		if people.is_empty() {
			return self.feed().await;
		}
		let href = &people[rand::random_range(..people.len())];
		info!("noise: {href}");
		self.behaviour.act(Action::Load).await?;
		let from = self.href().await?;
		self.tab
			.click(&format!(r#"{LINK}[href="{}"]:visible >> nth=0"#, href.replace('\\', r"\\").replace('"', r#"\""#)))
			.await?;
		if !self.moved(&from).await? {
			return self.home().await;
		}
		self.browse(rand::random_range(1..=5)).await
	}

	async fn search(&mut self) -> Result<()> {
		let query = QUERIES[rand::random_range(..QUERIES.len())];
		info!("noise: searching `{query}`");
		self.behaviour.act(Action::Load).await?;
		let from = self.href().await?;
		self.tab.type_into(SEARCH, query).await?;
		self.tab.press(SEARCH, "Enter").await?;
		if !self.moved(&from).await? {
			return self.home().await;
		}
		self.browse(rand::random_range(2..=6)).await
	}

	/// `n` wheel gestures, walked in two modes that each hold for a while: skimming, and reading a
	/// post with the pointer over it.
	async fn browse(&mut self, n: usize) -> Result<()> {
		let mut reading = false;
		for _ in 0..n {
			if rand::random_bool(0.25) {
				reading = !reading;
			}
			let over = match reading {
				true => self.readable().await?,
				false => None,
			};
			self.behaviour.act(Action::Scroll { seen: usize::from(over.is_some()) }).await?;
			let notches = match over {
				Some(_) => rand::random_range(1..=2u32),
				None => rand::random_range(1..=4u32),
			};
			self.tab.wheel(over.as_deref(), f64::from(notches) * 110.).await?;
			if over.is_some() {
				tokio::time::sleep(Duration::from_secs_f64(log_normal(10., 0.5).clamp(5., 25.))).await;
			}
		}
		Ok(())
	}

	/// A block of post text wholly on screen, to rest the pointer on.
	async fn readable(&mut self) -> Result<Option<String>> {
		let shown: Vec<usize> = self
			.tab
			.see(
				r#"sel => [...document.querySelectorAll(sel)].flatMap((e, i) => { const r = e.getBoundingClientRect(); return r.width > 0 && r.top >= 0 && r.bottom <= innerHeight && e.innerText.trim() ? [i] : [] })"#,
				READABLE,
			)
			.await?;
		Ok((!shown.is_empty()).then(|| format!("{READABLE} >> nth={}", shown[rand::random_range(..shown.len())])))
	}

	/// Whether the tab left `from` within 10 s and landed somewhere [`Tab::landed`](super::browser::Tab::landed) accepts.
	async fn moved(&mut self, from: &str) -> Result<bool> {
		let deadline = Instant::now() + Duration::from_secs(10);
		while self.href().await? == from {
			if Instant::now() >= deadline {
				info!("noise: still on {from} 10s after the click");
				return Ok(false);
			}
			tokio::time::sleep(Duration::from_millis(500)).await;
		}
		self.tab.landed().await
	}

	async fn home(&mut self) -> Result<()> {
		self.behaviour.act(Action::Load).await?;
		self.tab.goto(FEED).await
	}

	async fn href(&mut self) -> Result<String> {
		self.tab.see("location.href", ()).await
	}
}

/// Whether `href` leads to a person or a page rather than to a place on facebook.
fn somebody(href: &str) -> bool {
	let Ok(url) = reqwest::Url::parse(FEED).expect("a constant base").join(href) else {
		return false; // an href that is no URL leads nowhere a click should go
	};
	if url.host_str() != Some("www.facebook.com") {
		return false;
	}
	let segments: Vec<&str> = url.path_segments().expect("an https URL has a path").filter(|s| !s.is_empty()).collect();
	match segments[..] {
		["profile.php"] => url.query_pairs().any(|(k, _)| k == "id"),
		[name] => !RESERVED.contains(&name) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-') && !name.ends_with(".php"),
		_ => false,
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn only_people_and_pages_are_followed() {
		for href in [
			"/profile.php?id=100012345&__cft__[0]=x",
			"https://www.facebook.com/jean.dupont.92?__tn__=-]C",
			"/BoulangerieDuParc/",
			"https://www.facebook.com/333-255649711134087/?__cft__[0]=AZg",
		] {
			assert!(somebody(href), "{href}");
		}
		for href in [
			"/groups/123/",
			"/photo/?fbid=1",
			"/watch/",
			"/reel/123",
			"/jean.dupont/posts/pfbid0x",
			"/hashtag/lyon",
			"https://l.facebook.com/l.php?u=x",
			"/profile.php",
			"/permalink.php?story_fbid=1",
			"#",
			"/events/1",
			"/friends/suggestions/?profile_id=61576681210532",
			"?__cft__[0]=AZgglb",
		] {
			assert!(!somebody(href), "{href}");
		}
	}
}
