//! Skool publishes no API. Every page is Next.js SSR, so the whole payload sits in `__NEXT_DATA__`
//! and a plain GET is a complete read. Only `/auth/*` is gated behind an AWS-WAF JS challenge, so
//! cookies can be minted by a browser and by nothing else — but the browser stays off the read path
//! and runs about once per token rotation.
//!
//! Writes have nowhere to go but the undocumented REST API its own web client talks to.
//!
//! Everything a *profile* carries is public, so a [`Skool`] built without credentials is a working
//! reader rather than a degraded one. A *group* is not: logged out, both `/<group>` and
//! `/<group>/-/members` redirect to `/[group]/about`, so the venue axis needs `[skool]` credentials
//! and an actual membership.
//!
//! Nothing here listens. Skool is reached on demand and only by a human: `rolodex` for a person,
//! `recon` for a group.

use std::{
	collections::BTreeSet,
	io::Write as _,
	os::unix::fs::OpenOptionsExt as _,
	path::{Path, PathBuf},
	pin::pin,
	sync::LazyLock,
	time::{Duration, Instant},
};

use chromiumoxide::{Browser, BrowserConfig};
use color_eyre::eyre::{Result, WrapErr, bail, eyre};
use futures::{
	StreamExt as _,
	future::{Either, select},
};
use jiff::Timestamp;
use regex::Regex;
use reqwest::Method;
use serde::{Deserialize, Serialize};
use tokio::time;
use tracing::{info, instrument};
use v_utils::macros::MyConfigPrimitives;

use crate::reach::{Author, Direct, Item, Kind, Member, Page, Profile, Profiles, Source, Venue, VenueRef, VenueSource, Window};

const BASE: &str = "https://www.skool.com";
/// Everything the SSR payload cannot say, and every write. Cookie-authenticated, same as [`BASE`].
const API: &str = "https://api.skool.com";
/// Long enough for a slow WAF challenge, short enough that a wedged browser does not hold a daemon.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// A roster that keeps answering past this is one the pagination parameter is not steering.
const MEMBER_PAGES: usize = 200;
/// Read-only for a human, exactly like discord's connected accounts: none of these is a fetchable
/// [`Source`].
const LINKS: [(&str, &str); 5] = [
	("linkTwitter", "twitter"),
	("linkYoutube", "youtube"),
	("linkInstagram", "instagram"),
	("linkLinkedin", "linkedin"),
	("linkFacebook", "facebook"),
];

/// The whole of what `[skool]` carries: a session signs in, and nothing here watches anything.
#[derive(Clone, Debug, MyConfigPrimitives)]
pub struct SkoolCredentials {
	pub email: String,
	pub password: String,
}

pub struct Skool {
	http: reqwest::Client,
	cookie: Option<String>,
	creds: Option<SkoolCredentials>,
}

impl Skool {
	/// Picks up a cached cookie if one was ever minted. Its absence is a real state — the public
	/// reads work without it.
	pub fn try_new(creds: Option<SkoolCredentials>) -> Result<Self> {
		let path = cookie_path()?;
		let cookie = match path.exists() {
			true => {
				let cached: Cached = serde_json::from_str(&std::fs::read_to_string(&path).wrap_err_with(|| format!("reading {}", path.display()))?)
					.wrap_err_with(|| format!("{} is not a cookie cache — delete it to have one re-minted", path.display()))?;
				Some(cached.cookie)
			}
			false => None,
		};
		Ok(Self {
			// cloudfront 403s a request without a browser user-agent, cookies or no cookies
			http: reqwest::Client::builder().user_agent(UA).build()?,
			cookie,
			creds,
		})
	}

	/// The SSR payload skool embeds in every page. `["page"]` is the route it actually served, and
	/// classifying it is the caller's — `/[group]/about` in place of `/[group]` is how skool says
	/// "not a member". A session is minted first when there is one to mint.
	pub async fn page(&mut self, path: &str) -> Result<serde_json::Value> {
		let payload = self.fetch(path).await?;
		// `pageProps.self` is the signed-in viewer and rides on every route. The route does not answer
		// this: a signed-out group feed and a group we are simply not in both land on `/[group]/about`.
		// Cookie rotation is expected every few days, and only a browser can mint the next one.
		if payload.pointer("/props/pageProps/self").is_none_or(serde_json::Value::is_null) && self.creds.is_some() {
			self.refresh().await?;
			return self.fetch(path).await;
		}
		Ok(payload)
	}

	/// A group route, checked against the one skool says it served. `/[group]/about` in place of it is
	/// how skool spells "not a member", and no retry can change that.
	async fn group_page(&mut self, slug: &str, route: &str) -> Result<serde_json::Value> {
		let path = match route {
			"" => format!("/{slug}"),
			route => format!("/{slug}/{route}"),
		};
		let payload = self.page(&path).await?;
		let served = payload.get("page").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool served a page without a route"))?;
		if served.contains("/about") {
			bail!("skool `{slug}`: served {served} — a group is only readable by a signed-in member of it");
		}
		Ok(payload)
	}

	/// Skool has no global address book: a chat is *opened* through a group you are both in, and
	/// `chat-request` is a 400 anywhere else. A channel outlives the membership that opened it, so
	/// the ones already open are the first place to look and the only ones that survive leaving a
	/// group.
	async fn open_channel(&mut self, user: &str) -> Result<Option<String>> {
		// 30 is the page the web client asks for, and anything larger is a 400. Past it we fall through
		// to `chat-request`, which answers with the open channel anyway.
		let open = self.api(Method::GET, "/self/chat-channels", &[("limit", "30")], None).await?;
		let open: serde_json::Value = serde_json::from_str(&open).wrap_err("listing open chat channels")?;
		// `channels: null` is how skool spells an empty list
		let open = open.get("channels").ok_or_else(|| eyre!("a chat channel listing without `channels`: {open}"))?;
		open.as_array()
			.unwrap_or(&Vec::new())
			.iter()
			.find(|channel| {
				channel
					.pointer("/user_ids")
					.and_then(|ids| ids.as_array())
					.is_some_and(|ids| ids.iter().any(|id| id.as_str() == Some(user)))
			})
			.map(|channel| {
				channel
					.get("id")
					.and_then(|v| v.as_str())
					.map(str::to_string)
					.ok_or_else(|| eyre!("a chat channel without an id: {channel}"))
			})
			.transpose()
	}

	/// Which group the request goes through does not matter, and only skool knows which are shared,
	/// so they are tried until one answers. Opening a channel is a write, and stays off the read path.
	async fn request_channel(&mut self, user: &str) -> Result<String> {
		let groups: Vec<String> = self.my_groups().await?.into_iter().map(|(id, ..)| id).collect();
		let mut refused = Vec::with_capacity(groups.len());
		for group in groups {
			match self.api(Method::POST, &format!("/users/{user}/chat-request"), &[("g", &group)], None).await {
				Ok(opened) => {
					let opened: serde_json::Value = serde_json::from_str(&opened).wrap_err("opening a chat channel")?;
					return opened
						.pointer("/channel/id")
						.and_then(|v| v.as_str())
						.map(str::to_string)
						.ok_or_else(|| eyre!("a chat request came back without a channel: {opened}"));
				}
				Err(e) => refused.push(format!("{e:#}")),
			}
		}
		Err(eyre!("no group of mine opens a chat with them:\n{}", refused.join("\n")))
	}

	/// `(id, slug, display)` per group this session belongs to.
	async fn my_groups(&mut self) -> Result<Vec<(String, String, String)>> {
		let groups = self.api(Method::GET, "/self/groups", &[("limit", "50")], None).await?;
		let groups: serde_json::Value = serde_json::from_str(&groups).wrap_err("listing my groups")?;
		let groups = groups
			.get("groups")
			.and_then(|v| v.as_array())
			.ok_or_else(|| eyre!("a group listing without `groups`: {groups}"))?;
		groups
			.iter()
			.map(|group| {
				let id = group.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool group without an id: {group}"))?;
				let slug = group.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool group without a name: {group}"))?;
				let display = group.pointer("/metadata/displayName").and_then(|v| v.as_str()).unwrap_or(slug);
				Ok((id.to_string(), slug.to_string(), display.to_string()))
			})
			.collect()
	}

	async fn user_id(&mut self, handle: &str) -> Result<String> {
		let handle = handle.trim_start_matches('@');
		let profile = self.page(&format!("/@{handle}")).await?;
		Ok(profile
			.pointer("/props/pageProps/currentUser/id")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("no such skool handle: `{handle}`"))?
			.to_string())
	}

	/// Unlike the SSR pages, which answer a dead session by serving the signed-out view, the API says
	/// 401 — so that, rather than the payload, is what rotation hangs off here.
	async fn api(&mut self, method: Method, path: &str, query: &[(&str, &str)], body: Option<serde_json::Value>) -> Result<String> {
		let mut response = self.send_api(method.clone(), path, query, body.as_ref()).await?;
		if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.creds.is_some() {
			self.refresh().await?;
			response = self.send_api(method.clone(), path, query, body.as_ref()).await?;
		}
		let status = response.status();
		let payload = response.text().await?;
		if !status.is_success() {
			bail!("{method} {path} answered {status}: {payload}");
		}
		Ok(payload)
	}

	async fn send_api(&self, method: Method, path: &str, query: &[(&str, &str)], body: Option<&serde_json::Value>) -> Result<reqwest::Response> {
		let mut request = self.http.request(method.clone(), format!("{API}{path}")).query(query);
		if let Some(cookie) = &self.cookie {
			request = request.header(reqwest::header::COOKIE, cookie);
		}
		// a bodyless POST still has to declare itself json, or the API answers 415
		request = match body {
			Some(body) => request.json(body),
			None => request.header(reqwest::header::CONTENT_TYPE, "application/json"),
		};
		request.send().await.wrap_err_with(|| format!("{method} {path}"))
	}

	async fn fetch(&self, path: &str) -> Result<serde_json::Value> {
		let mut request = self.http.get(format!("{BASE}{path}"));
		if let Some(cookie) = &self.cookie {
			request = request.header(reqwest::header::COOKIE, cookie);
		}
		let html = request.send().await.wrap_err_with(|| format!("GET {path}"))?.error_for_status()?.text().await?;
		next_data(&html)
	}

	/// Drives a headless chromium through the login form, because `/auth/login` answers a direct POST
	/// with a CloudFront 403 until an AWS-WAF challenge has been solved in a JS runtime.
	#[instrument(skip_all)]
	async fn refresh(&mut self) -> Result<()> {
		let creds = self.creds.clone().expect("every caller checks for credentials before refreshing");
		info!("minting a fresh skool cookie");
		let config = BrowserConfig::builder().build().map_err(|e| eyre!("chromium config: {e}"))?;
		let (browser, mut handler) = Browser::launch(config).await?;

		// nothing on `browser` resolves unless the CDP stream is drained alongside it
		let login = pin!(login(&browser, &creds));
		let drain = pin!(async { while handler.next().await.is_some() {} });
		let cookies = match select(login, drain).await {
			Either::Left((cookies, _)) => cookies,
			Either::Right(((), _)) => Err(eyre!("the chromium CDP handler exited during login")),
		}?;

		let path = cookie_path()?;
		let mut file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
		file.write_all(serde_json::to_string(&Cached { cookie: cookies.clone() })?.as_bytes())?;
		self.cookie = Some(cookies);
		Ok(())
	}
}

impl Profiles for Skool {
	/// The profile fields skool serves to anybody. Their absence of a session is why the person axis
	/// needs no credentials at all: `postTrees` is the only part membership adds, and it comes back
	/// empty rather than failing.
	async fn profile(&mut self, handle: &str, window: Window) -> Result<Profile> {
		let handle = handle.trim_start_matches('@');
		let payload = self.page(&format!("/@{handle}")).await?;
		let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("skool served a page without pageProps"))?;
		let user = props.get("currentUser").ok_or_else(|| eyre!("no such skool handle: `{handle}`"))?;

		let mut profile = Profile::default();
		let metadata = user.get("metadata");
		let field = |name: &str| metadata.and_then(|m| m.get(name)).and_then(|v| v.as_str());
		profile.state("skool:bio", field("bio"));
		profile.state("skool:location", field("location"));
		for (link, platform) in LINKS {
			if let Some(name) = field(link).and_then(handle_from_link) {
				profile.handles.insert(platform.to_string(), name);
			}
		}
		profile.display = full_name(user, handle);
		for group in user.pointer("/profileData/groupsMemberOf").and_then(|v| v.as_array()).into_iter().flatten() {
			let slug = group.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool group without a name: {group}"))?;
			profile.venues.push(VenueRef {
				platform: VenueSource::Skool,
				slug: slug.to_string(),
				display: group.pointer("/metadata/displayName").and_then(|v| v.as_str()).unwrap_or(slug).to_string(),
			});
		}

		// newest-first, and only ever populated for a session that shares a group with them
		let posts = props.get("postTrees").and_then(|v| v.as_array()).ok_or_else(|| eyre!("skool `{handle}`: no postTrees"))?;
		profile.activity = page_of(posts, &window, Kind::Activity, |_| Author::Handle(handle.to_string()))?;
		Ok(profile)
	}
}

impl Direct for Skool {
	/// Reading skool's chat is not implemented. Nothing dispatches a read here — `rolodex::pull` sends
	/// skool through [`Profiles`] alone — and this bails rather than answering empty, so a caller that
	/// starts to finds out.
	async fn direct(&mut self, _handle: &str, _window: Window, _assets: &Path) -> Result<Page> {
		bail!("skool chat is write-only here — see `Source::has_direct`")
	}

	/// Skool's chat lives behind the one thing its SSR pages are not: a REST API at [`API`]. The
	/// handle is public, the id it resolves to is what every chat route speaks.
	async fn send(&mut self, handle: &str, text: &str) -> Result<()> {
		let user = self.user_id(handle).await?;
		let channel = match self.open_channel(&user).await? {
			Some(channel) => channel,
			None => self.request_channel(&user).await.wrap_err_with(|| format!("no chat to send to `{handle}` over"))?,
		};
		// `ct` is the client the message was typed in; the web chat calls itself `wdc`
		self.api(
			Method::POST,
			&format!("/channels/{channel}/messages"),
			&[("ct", "wdc")],
			Some(serde_json::json!({ "content": text })),
		)
		.await
		.map(|_| ())
	}
}

impl Venue for Skool {
	async fn venues(&mut self) -> Result<Vec<VenueRef>> {
		Ok(self
			.my_groups()
			.await?
			.into_iter()
			.map(|(_, slug, display)| VenueRef {
				platform: VenueSource::Skool,
				slug,
				display,
			})
			.collect())
	}

	async fn members(&mut self, at: &VenueRef) -> Result<Vec<Member>> {
		let mut out = Vec::new();
		let mut seen = BTreeSet::new();
		//LOOP: pages the roster until one adds nobody, capped by `MEMBER_PAGES`
		for page in 1..=MEMBER_PAGES {
			let payload = self.group_page(&at.slug, &format!("-/members?p={page}")).await?;
			let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("skool served a member page without pageProps"))?;
			let users = props.get("users").and_then(|v| v.as_array()).ok_or_else(|| {
				eyre!(
					"skool `{}`: the member page carries no `users` — it holds {:?}. \
					 `cargo r -p social_networks_adapters --example skool_probe -- {} members` dumps the payload; point this at the right key.",
					at.slug,
					props.as_object().map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default(),
					at.slug
				)
			})?;
			let before = out.len();
			for user in users {
				let handle = user.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool member without a name: {user}"))?;
				if !seen.insert(handle.to_string()) {
					continue;
				}
				out.push(Member {
					display: full_name(user, handle).expect("`full_name` falls back to the handle"),
					handle: handle.to_string(),
					joined: user
						.get("createdAt")
						.and_then(|v| v.as_str())
						.map(str::parse::<Timestamp>)
						.transpose()
						.wrap_err("skool timestamps are RFC3339")?,
				});
			}
			// a pagination parameter skool ignores serves page 1 forever
			if out.len() == before {
				break;
			}
		}
		Ok(out)
	}

	/// The group feed, which is the same `postTrees` array a profile carries — one parse for both.
	async fn posts(&mut self, at: &VenueRef, window: Window, _assets: &Path) -> Result<Page> {
		let payload = self.group_page(&at.slug, "").await?;
		let posts = payload
			.pointer("/props/pageProps/postTrees")
			.and_then(|v| v.as_array())
			.ok_or_else(|| eyre!("skool `{}`: no postTrees", at.slug))?;
		let slug = at.slug.clone();
		page_of(posts, &window, Kind::Post, |node| {
			// the author rides on the tree node; a post whose author skool withheld is still the group's
			Author::Handle(
				node.pointer("/user/name")
					.or_else(|| node.pointer("/post/user/name"))
					.and_then(|v| v.as_str())
					.unwrap_or(&slug)
					.to_string(),
			)
		})
	}
}

/// One `postTrees` array, newest-first, turned into a page: a group feed and a profile serve the
/// same nodes, so they take the same parse.
///
/// Ids are opaque hex, so the cursor can only be *recognised*, not compared — a post that has already
/// scrolled off the first page is reported again rather than missed.
fn page_of(nodes: &[serde_json::Value], window: &Window, kind: Kind, attribute: impl Fn(&serde_json::Value) -> Author) -> Result<Page> {
	let after = match window {
		Window::Above { after, .. } => after.as_deref(),
		// a skool feed is a snapshot: there is nothing under the first page to walk down to
		Window::Below { .. } => return Ok(Page { exhausted: true, ..Page::default() }),
	};
	let mut page = Page { exhausted: true, ..Page::default() };
	for node in nodes {
		let post = node.get("post").ok_or_else(|| eyre!("a skool postTree without a post: {node}"))?;
		let id = post.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool post without an id: {post}"))?;
		if after == Some(id) {
			break;
		}
		page.newest.get_or_insert_with(|| id.to_string());
		let created = post.get("createdAt").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without createdAt"))?;
		let at: Timestamp = created.parse().wrap_err("skool timestamps are RFC3339")?;
		if window.reached(at) {
			break;
		}
		let title = post.pointer("/metadata/title").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without a title"))?;
		let (group, name) = (post.pointer("/group/name").and_then(|v| v.as_str()), post.get("name").and_then(|v| v.as_str()));
		let (Some(group), Some(name)) = (group, name) else {
			bail!("skool post {id} carries no group/name to build a permalink from: {post}");
		};
		let body = post.pointer("/metadata/content").and_then(|v| v.as_str()).unwrap_or_default().trim();
		page.oldest = Some(id.to_string());
		page.items.push(Item {
			id: id.to_string(),
			source: Source::Skool,
			at,
			kind,
			author: attribute(node),
			text: match body.is_empty() {
				true => title.to_string(),
				false => format!("{title}\n{body}"),
			},
			attachments: Vec::new(),
			permalink: Some(format!("https://www.skool.com/{group}/{name}")),
		});
		if page.items.len() >= window.limit() {
			break;
		}
	}
	page.items.reverse();
	Ok(page)
}

/// Skool keeps the two halves apart and prints neither on its own. The handle is what is left when
/// somebody filled in no name at all.
fn full_name(user: &serde_json::Value, handle: &str) -> Option<String> {
	let named = |key: &str| user.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty());
	match (named("firstName"), named("lastName")) {
		(Some(first), Some(last)) => Some(format!("{first} {last}")),
		(first, last) => Some(first.or(last).unwrap_or(handle).to_string()),
	}
}

/// A session cookie is a bearer credential, so the file it lives in is `0600`.
#[derive(Deserialize, Serialize)]
struct Cached {
	cookie: String,
}

/// Closing over CDP would end the handler stream this is selected against, so the browser is left to
/// `Drop`, which kills the child.
async fn login(browser: &Browser, creds: &SkoolCredentials) -> Result<String> {
	let page = browser.new_page(format!("{BASE}/login")).await?;
	page.find_element("input#email").await?.click().await?.type_str(&creds.email).await?;
	page.find_element("input#password")
		.await?
		.click()
		.await?
		.type_str(&creds.password)
		.await?
		.press_key("Enter")
		.await?;

	// the form navigates away on success and re-renders in place on a rejected password, so the URL is
	// the only signal that separates the two
	let deadline = Instant::now() + LOGIN_TIMEOUT;
	//LOOP: polls until the frame commits a navigation, bounded by `deadline`
	let url = loop {
		// `None` is a frame that has not committed a navigation yet, which is not somewhere to be
		match page.url().await? {
			Some(url) if !url.contains("/login") => break url,
			url =>
				if Instant::now() >= deadline {
					bail!("still on {url:?} {LOGIN_TIMEOUT:?} after submitting the login form");
				},
		}
		time::sleep(Duration::from_millis(500)).await;
	};
	info!("skool login landed on {url}");

	let header = page
		.get_cookies()
		.await?
		.iter()
		.filter(|c| c.domain.contains("skool.com"))
		.map(|c| format!("{}={}", c.name, c.value))
		.collect::<Vec<_>>()
		.join("; ");
	if header.is_empty() {
		bail!("login navigated to {url} but left no skool.com cookies");
	}
	Ok(header)
}

fn cookie_path() -> Result<PathBuf> {
	Ok(xdg::BaseDirectories::with_prefix("social_networks").place_state_file("skool_cookies.json")?)
}

/// Deliberately the HTML rather than `/_next/data/<buildId>/…`: that route needs a `buildId` that
/// rotates weekly, and it measures larger.
fn next_data(html: &str) -> Result<serde_json::Value> {
	static NEXT_DATA: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"(?s)<script id="__NEXT_DATA__" type="application/json">(.*?)</script>"#).expect("static pattern"));
	let json = NEXT_DATA.captures(html).ok_or_else(|| eyre!("no __NEXT_DATA__ in the served page"))?;
	Ok(serde_json::from_str(json.get(1).expect("the pattern has one group").as_str())?)
}

/// The last path segment of a profile URL, which is the handle on every platform skool links to.
/// `None` for the empty string skool stores for a link nobody set, and for a bare domain.
fn handle_from_link(url: &str) -> Option<String> {
	let path = url.split(['?', '#']).next().expect("a split yields at least one piece");
	let segment = path.trim_end_matches('/').rsplit('/').next().expect("a split yields at least one piece");
	(!segment.is_empty() && !segment.contains('.')).then(|| segment.to_string())
}

#[cfg(test)]
mod tests {
	use super::*;

	/// The payload is the last thing on the page and carries markup of its own, so the pattern has to
	/// be non-greedy and has to span lines.
	#[test]
	fn next_data_is_the_whole_payload() {
		let html = r#"<html><body><div id="__next">a</div><script id="__NEXT_DATA__" type="application/json">{"page":"/[group]",
			"props":{"pageProps":{"postTrees":[]}}}</script><script src="x.js"></script></body></html>"#;
		let payload = next_data(html).unwrap();
		assert_eq!(payload["page"], "/[group]");
		assert!(payload.pointer("/props/pageProps/postTrees").unwrap().as_array().unwrap().is_empty());
		assert!(next_data("<html><body>no payload</body></html>").is_err());
	}

	/// Skool stores an unset link as `""` rather than omitting it, and writes the ones it does hold
	/// back in whatever shape the person pasted.
	#[test]
	fn a_link_is_not_a_handle() {
		assert_eq!(handle_from_link(""), None);
		assert_eq!(handle_from_link("https://twitter.com"), None);
		assert_eq!(handle_from_link("https://x.com/valeratrades/"), Some("valeratrades".to_string()));
		assert_eq!(handle_from_link("https://www.youtube.com/@skool-news?sub_confirmation=1"), Some("@skool-news".to_string()));
		assert_eq!(handle_from_link("https://www.linkedin.com/in/somebody#about"), Some("somebody".to_string()));
	}

	/// The cursor is recognised rather than compared, so a re-read of an unchanged feed carries
	/// nothing — which is what keeps `recon posts` idempotent over one window.
	#[test]
	fn a_feed_stops_at_the_cursor() {
		let node = |id: &str, at: &str| {
			serde_json::json!({
				"user": {"name": "lory"},
				"post": {"id": id, "createdAt": at, "name": "a-post", "group": {"name": "g"}, "metadata": {"title": "t"}}
			})
		};
		let nodes = [node("c", "2026-03-03T00:00:00Z"), node("b", "2026-03-02T00:00:00Z"), node("a", "2026-03-01T00:00:00Z")];

		let all = page_of(&nodes, &Window::above(None), Kind::Post, |_| Author::Me).unwrap();
		assert_eq!(all.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"], "oldest-first");
		assert_eq!(all.newest.as_deref(), Some("c"));
		assert_eq!(all.items[0].permalink.as_deref(), Some("https://www.skool.com/g/a-post"));

		let since = page_of(&nodes, &Window::above(Some("b".to_string())), Kind::Post, |_| Author::Me).unwrap();
		assert_eq!(since.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["c"]);

		let dated = page_of(&nodes, &Window::since("2026-03-02T00:00:00Z".parse().unwrap()), Kind::Post, |_| Author::Me).unwrap();
		assert_eq!(dated.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
	}

	/// The name skool prints is two fields it never joins itself — the gap that had a person file
	/// named by hand.
	#[test]
	fn a_display_name_is_two_fields() {
		let user = serde_json::json!({"firstName": "Lory", "lastName": "Bellardant"});
		assert_eq!(full_name(&user, "lory-bellardant-1253").as_deref(), Some("Lory Bellardant"));
		assert_eq!(full_name(&serde_json::json!({"firstName": "Lory"}), "l").as_deref(), Some("Lory"));
		assert_eq!(full_name(&serde_json::json!({"firstName": ""}), "l").as_deref(), Some("l"));
	}
}
