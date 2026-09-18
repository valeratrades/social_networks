//! Skool publishes no API. Every page is Next.js SSR, so the whole payload sits in `__NEXT_DATA__`
//! and a plain GET is a complete read. Only `/auth/*` is gated behind an AWS-WAF JS challenge, so
//! cookies can be minted by a browser and by nothing else — but the session it leaves behind is a
//! year-long JWT, so the browser stays off the read path and runs about once a year.
//!
//! Writes have nowhere to go but the undocumented REST API its own web client talks to.
//!
//! Everything a *profile* carries is public, so a [`Skool`] built without credentials is a working
//! reader rather than a degraded one. A *group* is not: logged out, both `/<group>` and
//! `/<group>/-/members` redirect to `/[group]/about`, so the venue axis needs `[skool]` credentials
//! and an actual membership.
//!
//! Skool is reached on demand and only by a human — `rolodex` for a person, `recon` for a group,
//! [`Skool::classroom`] for the course a group teaches — with one exception: [`SkoolDms`] polls the
//! chat listing so a `/ping` here lands like one anywhere else.

use std::{
	collections::{BTreeMap, HashMap, HashSet},
	convert::Infallible,
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
use tokio::{sync::mpsc::UnboundedSender, time};
use tracing::{info, instrument, warn};
use v_utils::macros::MyConfigPrimitives;

use crate::{
	client::{AdapterError, Client},
	dm_event::DmEvent,
	reach::{Author, Direct, Item, Kind, Member, Page, Profile, Profiles, Source, Venue, VenueRef, VenueSource, Window},
};

const SURFACE: &str = "skool_dms";
/// What a [`DmEvent`] from here calls itself, and what a `{skool = "..."}` monitored user matches on.
const PLATFORM: &str = "Skool";
/// Skool pushes nothing, so this is the whole of how late a `/ping` can be.
const POLL: Duration = Duration::from_secs(60);
/// A cookie rotation costs one poll and a CloudFront block a few, so the daemon only comes down once
/// waiting has stopped being an explanation.
const POLL_FAILURES: usize = 5;
/// How much of a channel skool has only just served counts as new. Small, because the alternative to
/// guessing here is replaying a conversation nobody asked to see again.
const CATCH_UP: usize = 5;
const BASE: &str = "https://www.skool.com";
/// Everything the SSR payload cannot say, and every write. Cookie-authenticated, same as [`BASE`].
const API: &str = "https://api.skool.com";
/// Long enough for a slow WAF challenge, short enough that a wedged browser does not hold a daemon.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
/// Skool publishes no rate limit, and CloudFront answers a burst with a 403 block page rather than a
/// status worth classifying — measured: a few hundred calls back to back trips it, and it lifts on
/// its own a minute or two later. A roster sweep is the only thing here that makes more than a
/// handful of calls, so it paces itself rather than find out again.
const PACE: Duration = Duration::from_millis(700);
/// Doubling from [`PACE`], this waits about two minutes out in total — longer than the block above
/// was measured to last. A read still refused after it is not being throttled.
const READ_RETRIES: usize = 7;
/// The largest `before`/`after` skool's chat answers — past it, `invalid before: <n>`.
const CHAT_PAGE: usize = 50;
/// The largest `limit` the member search answers: `11..=49` are `invalid limit: <n>` and `50` up is
/// a 422. There is no cursor either, so ten is the whole of what one term can reach.
const SEARCH_PAGE: usize = 10;
/// Skool answers a scripted DM with `200` and a shadowban: the message sits in our own thread and
/// reaches nobody, and the account it was sent from stays that way.
const SEND: bool = false;
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
		// `auth_token` is a year-long JWT, so this is a rare path — and only a browser can mint the next
		// one, since `/auth/login` answers a plain POST with a CloudFront 403 whatever cookies it carries.
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
			// the feed is the group's own route under a query, not a route of its own
			query if query.starts_with('?') => format!("/{slug}{query}"),
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
		self.chat_channels()
			.await?
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

	/// Every chat channel this session has open. `user` on each is the other party and `last_message_id`
	/// what they last said, so a listing answers who wrote and whether it is new without touching a
	/// conversation. 30 is the page the web client asks for, and anything larger is a 400.
	async fn chat_channels(&mut self) -> Result<Vec<serde_json::Value>> {
		let open = self.api(Method::GET, "/self/chat-channels", &[("limit", "30")], None).await?;
		let open: serde_json::Value = serde_json::from_str(&open).wrap_err("listing open chat channels")?;
		let channels = open.get("channels").ok_or_else(|| eyre!("a chat channel listing without `channels`: {open}"))?;
		// `channels: null` is how skool spells an empty list
		Ok(channels.as_array().cloned().unwrap_or_default())
	}

	/// Skool pages its chat around a message rather than from an end: `msg` is the pivot, `before` and
	/// `after` how many to either side of it, and the pivot itself always comes back — so it is
	/// stripped here and only ever used as a cursor. Without `msg` the pivot is the newest message.
	/// The bool is whether anything remains on the side asked for.
	async fn chat_page(&mut self, channel: &str, pivot: Option<&str>, side: Side, count: usize) -> Result<(Vec<serde_json::Value>, bool)> {
		assert!((1..=CHAT_PAGE).contains(&count), "skool answers `{count}` for a count outside 1..={CHAT_PAGE}");
		let count = count.to_string();
		let mut query = vec![(side.as_str(), count.as_str())];
		if let Some(pivot) = pivot {
			query.push(("msg", pivot));
		}
		let payload = self.api(Method::GET, &format!("/channels/{channel}/messages"), &query, None).await?;
		let payload: serde_json::Value = serde_json::from_str(&payload).wrap_err("listing chat messages")?;
		let more = payload
			.get(side.more())
			.and_then(serde_json::Value::as_bool)
			.ok_or_else(|| eyre!("a skool chat page without `{}`: {payload}", side.more()))?;
		// `messages: null` is how skool spells an empty list, same as `channels` above
		let mut messages: Vec<serde_json::Value> = payload
			.get("messages")
			.ok_or_else(|| eyre!("a skool chat page without `messages`: {payload}"))?
			.as_array()
			.cloned()
			.unwrap_or_default();
		if let Some(pivot) = pivot {
			messages.retain(|m| m.get("id").and_then(|v| v.as_str()) != Some(pivot));
		}
		Ok((messages, more))
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
		// every group of ours has answered about them, so this is their state and not ours
		Err(crate::reach::Unreachable(format!("no group of mine opens a chat with them:\n{}", refused.join("\n"))).into())
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

	/// The search behind the group's member bar. Matches a prefix of any word of a handle or a
	/// display name, answers at most [`SEARCH_PAGE`], and carries no cursor — so it is a lookup and
	/// not a way to walk a roster.
	///
	/// It is the only read here that reaches the members [`Venue::members`] cannot: a member with no
	/// map pin is off the roster entirely, and search still finds them. The user objects it answers
	/// carry no `member`, so [`Member::joined`] is `None` where the member page would have stated it.
	pub async fn find(&mut self, at: &VenueRef, term: &str) -> Result<Vec<Member>> {
		let group = self.group_id(&at.slug).await?;
		let body = serde_json::json!({ "query": term, "group_id": group, "limit": SEARCH_PAGE });
		let payload = self.api(Method::POST, "/search/users", &[], Some(body)).await?;
		let payload: serde_json::Value = serde_json::from_str(&payload).wrap_err("searching members")?;
		// `users: null` is how skool spells an empty list, same as `channels` and `messages`
		let users = payload.get("users").ok_or_else(|| eyre!("a skool member search without `users`: {payload}"))?;
		let found = users.as_array().map(Vec::as_slice).unwrap_or_default().iter().map(member).collect::<Result<Vec<_>>>()?;
		if found.len() == SEARCH_PAGE {
			warn!("skool `{}`: `{term}` filled the page of {SEARCH_PAGE}, so it has more — narrow the term", at.slug);
		}
		Ok(found)
	}

	/// The 32-hex id the API addresses a group by, from the slug a [`VenueRef`] spells. Only a group
	/// this session is in has one to find, which is the same condition every other read here carries.
	async fn group_id(&mut self, slug: &str) -> Result<String> {
		self.my_groups()
			.await?
			.into_iter()
			.find(|(_, name, _)| name == slug)
			.map(|(id, ..)| id)
			.ok_or_else(|| eyre!("skool `{slug}`: not a group this session is in, so it has no id to address"))
	}

	/// The first page of the member list, keyed by user id, and how many members the group says it
	/// has. Everything here is free; everything past it costs a request per person.
	async fn listed(&mut self, at: &VenueRef) -> Result<(BTreeMap<String, Member>, usize)> {
		let payload = self.group_page(&at.slug, "-/members").await?;
		let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("skool served a member page without pageProps"))?;
		let users = props.get("users").and_then(|v| v.as_array()).ok_or_else(|| {
			eyre!(
				"skool `{}`: the member page carries no `users` — it holds {:?}. \
				 `cargo r -p social_networks_adapters --example skool_probe -- {} -/members` dumps the payload; point this at the right key.",
				at.slug,
				props.as_object().map(|o| o.keys().collect::<Vec<_>>()).unwrap_or_default(),
				at.slug
			)
		})?;
		let total = props.get("totalMembers").and_then(serde_json::Value::as_u64).unwrap_or(users.len() as u64) as usize;

		let mut roster = BTreeMap::new();
		for user in users {
			let id = user.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool member without an id: {user}"))?;
			roster.insert(id.to_string(), member(user)?);
		}
		Ok((roster, total))
	}

	/// One post's replies. `children` on the rendered post route is always empty — the web client
	/// fills it from here — so this is the only place a reply exists. Nesting is flattened: a reply to
	/// a reply is still something somebody said in the group, and a transcript orders by time.
	async fn replies(&mut self, post: &str, group: &str, permalink: &str) -> Result<Vec<Item>> {
		let payload = self.api(Method::GET, &format!("/posts/{post}/comments"), &[("group-id", group)], None).await?;
		let payload: serde_json::Value = serde_json::from_str(&payload).wrap_err_with(|| format!("skool post {post} replies are not json"))?;
		let mut out = Vec::new();
		replies_of(payload.pointer("/post_tree/children"), permalink, &mut out)?;
		Ok(out)
	}

	/// One member off the API, which spells the same fields in snake_case that the SSR pages spell in
	/// camelCase. The only way to a handle for somebody the member page never served.
	async fn user(&mut self, id: &str) -> Result<Member> {
		let payload = self.api(Method::GET, &format!("/users/{id}"), &[], None).await?;
		member(&serde_json::from_str(&payload).wrap_err_with(|| format!("skool user {id} is not json"))?)
	}

	/// A group's map, keyed by user id. Skool renders the pins out of a signed blob on its CDN rather
	/// than out of the page, so the page is read for the URL and the URL for the data — and it is
	/// served gzipped, which is why `reqwest` carries that feature.
	///
	/// Every pin is offset by 10+ miles, which skool says outright. Empty for a group with the map
	/// turned off, and for the members who never gave a location — 325 pins over 406 members here.
	async fn pins(&mut self, at: &VenueRef) -> Result<HashMap<String, (f64, f64)>> {
		let payload = self.group_page(&at.slug, "-/map").await?;
		let Some(url) = payload.pointer("/props/pageProps/dataUrl").and_then(|v| v.as_str()) else {
			warn!("skool `{}`: no map on the group, so no member carries a position", at.slug);
			return Ok(HashMap::new());
		};
		// the URL is signed, so it takes the cookie no more than it takes the user agent
		let pins: Vec<Pin> = self
			.http
			.get(url)
			.send()
			.await?
			.error_for_status()?
			.json()
			.await
			.wrap_err("the skool map blob is a list of pins")?;
		pins.into_iter().map(|pin| Ok((pin.u, (pin.p[0], pin.p[1])))).collect()
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

	/// The classroom, whole. The index names the courses and nothing under them; and a course's own
	/// route carries its lessons, but a lesson only ever arrives *whole* on the route that selects it
	/// — everywhere else its body and its video are simply absent, so a read off the course route
	/// alone silently loses whatever a lesson had written under its video. So this is one request per
	/// course to learn the order, and one per lesson to read it, which is what keeps it hand-run like
	/// the rest of the venue axis.
	///
	/// The tree comes out as the tree, rather than flattened: a course is a page of its own, with its
	/// own body and its own address, and the order lessons sit in is the order they are meant to be
	/// taken in.
	pub async fn classroom(&mut self, at: &VenueRef) -> Result<Vec<Course>> {
		let index = self.group_page(&at.slug, "classroom").await?;
		let courses = index
			.pointer("/props/pageProps/allCourses")
			.and_then(|v| v.as_array())
			.ok_or_else(|| eyre!("skool `{}`: the classroom serves no `allCourses`", at.slug))?
			.clone();

		let mut out = Vec::new();
		for course in &courses {
			// the 8-hex `name`, not the 32-hex `id`, is what `/classroom/<x>` addresses
			let name = course.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool course without a name: {course}"))?;
			let title = course
				.pointer("/metadata/title")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("skool course `{name}` without a title: {course}"))?;
			let updated = course
				.get("updatedAt")
				.and_then(|v| v.as_str())
				.ok_or_else(|| eyre!("skool course `{name}` without an updatedAt: {course}"))?;
			time::sleep(PACE).await;
			let payload = self.group_page(&at.slug, &format!("classroom/{name}")).await?;
			let mut found = Vec::new();
			lessons_of(payload.pointer("/props/pageProps/course/children"), title, &mut found)?;
			info!("skool `{}`: `{title}`, {} lessons", at.slug, found.len());

			let mut lessons = Vec::new();
			for (id, module) in found {
				time::sleep(PACE).await;
				let payload = self.group_page(&at.slug, &format!("classroom/{name}?md={id}")).await?;
				let node = node_of(payload.pointer("/props/pageProps/course/children"), &id).ok_or_else(|| eyre!("skool lesson {id} is not in the course tree its own route serves"))?;
				let (mut lesson, hosted) = lesson_of(&node, &at.slug, name, module)?;
				if let Some(hosted) = hosted {
					let video = payload
						.pointer("/props/pageProps/video")
						.ok_or_else(|| eyre!("skool lesson {id} claims a video skool hosts, and its own route serves none"))?;
					assert_eq!(video.get("id").and_then(|v| v.as_str()), Some(hosted.as_str()), "skool served the wrong video for lesson {id}");
					lesson.video = Some(mux(video)?);
				}
				lessons.push(lesson);
			}
			out.push(Course {
				id: name.to_string(),
				title: title.to_string(),
				permalink: format!("{BASE}/{}/classroom/{name}", at.slug),
				at: updated.parse().wrap_err("skool timestamps are RFC3339")?,
				body: course.pointer("/metadata/desc").and_then(|v| v.as_str()).map(rich_text).transpose()?.unwrap_or_default(),
				lessons,
			});
		}
		Ok(out)
	}

	/// Unlike the SSR pages, which answer a dead session by serving the signed-out view, the API says
	/// 401 — so that, rather than the payload, is what rotation hangs off here.
	async fn api(&mut self, method: Method, path: &str, query: &[(&str, &str)], body: Option<serde_json::Value>) -> Result<String> {
		let mut response = self.send_api(method.clone(), path, query, body.as_ref()).await?;
		if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.creds.is_some() {
			self.refresh().await?;
			response = self.send_api(method.clone(), path, query, body.as_ref()).await?;
		}
		// A GET is idempotent, so a refusal is worth waiting out; a write is not, and one that lands
		// twice is worse than one that fails.
		let mut backoff = PACE * 2;
		//LOOP: bounded by `READ_RETRIES`
		for _ in 0..READ_RETRIES {
			if response.status().is_success() || method != Method::GET {
				break;
			}
			warn!("skool: {method} {path} answered {}, holding off {backoff:?}", response.status());
			time::sleep(backoff).await;
			backoff *= 2;
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
		// the gap that had a person file named by hand: skool keeps the two halves apart and joins
		// neither, so nothing downstream ever saw a name
		profile.state("skool:name", full_name(user).as_deref());
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
		profile.activity = page_of(posts, &window, Kind::Activity, None, |_| Author::Handle(handle.to_string()))?;
		Ok(profile)
	}
}

impl Direct for Skool {
	/// A handle whose chat was never opened has an empty conversation rather than an unreadable one:
	/// opening one is [`request_channel`](Skool::request_channel), which is a write.
	async fn direct(&mut self, handle: &str, window: Window, _assets: &Path) -> Result<Page> {
		let user = self.user_id(handle).await?;
		let Some(channel) = self.open_channel(&user).await? else {
			return Ok(Page { exhausted: true, ..Page::default() });
		};
		let limit = window.limit();
		// oldest-first throughout, which is the order skool answers in and the order a `Page` wants
		let mut raw: Vec<serde_json::Value> = Vec::new();
		let mut exhausted = false;

		match &window {
			// one page under the floor, checked in by the caller before it asks for the next
			Window::Below { before, .. } => {
				let (page, more) = self.chat_page(&channel, before.as_deref(), Side::Before, CHAT_PAGE.min(limit)).await?;
				exhausted = !more;
				raw = page;
			}
			// forward from the checkpoint, which is the pivot the first page hangs off
			Window::Above { after: Some(after), .. } => {
				let mut pivot = after.clone();
				//LOOP: walks strictly upwards from a fixed checkpoint towards the newest message
				loop {
					let (page, more) = self.chat_page(&channel, Some(&pivot), Side::After, CHAT_PAGE).await?;
					let Some(newest) = page.last().map(message_id).transpose()?.map(str::to_string) else { break };
					raw.extend(page);
					if !more {
						break;
					}
					if raw.len() >= limit {
						warn!("skool `{handle}`: stopping at {limit} messages, the rest comes on the next pull");
						break;
					}
					pivot = newest;
				}
			}
			// walk backwards from the newest, since there is no floor to walk up from
			Window::Above { after: None, .. } => {
				let mut pivot: Option<String> = None;
				//LOOP: bounded by `limit` and by the conversation, which is walked strictly downwards
				while raw.len() < limit {
					let (page, more) = self.chat_page(&channel, pivot.as_deref(), Side::Before, CHAT_PAGE.min(limit - raw.len())).await?;
					let Some(oldest) = page.first().map(message_id).transpose()?.map(str::to_string) else {
						exhausted = true;
						break;
					};
					raw.splice(0..0, page);
					if !more {
						exhausted = true;
						break;
					}
					pivot = Some(oldest);
				}
			}
		}

		let mut items = Vec::with_capacity(raw.len());
		for message in &raw {
			items.push(chat_item(message, handle, &user, &channel)?);
		}
		items.retain(|item| !window.reached(item.at));
		items.sort_by_key(|item| item.at);
		Ok(Page {
			newest: raw.last().map(message_id).transpose()?.map(str::to_string),
			oldest: raw.first().map(message_id).transpose()?.map(str::to_string),
			exhausted,
			items,
		})
	}

	/// Skool's chat lives behind the one thing its SSR pages are not: a REST API at [`API`]. The
	/// handle is public, the id it resolves to is what every chat route speaks.
	async fn send(&mut self, handle: &str, text: &str) -> Result<()> {
		if !SEND {
			bail!("skool shadowbans the account a scripted DM goes out from, so `{handle}` is written to by hand or not at all:\n{text}");
		}
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

	/// The roster, which skool serves in two halves and pages through in neither.
	///
	/// The member page is the only thing that states a *group* join date, but `?p=` is echoed back
	/// into `page` and otherwise ignored, so it never hands over more than its first 30. The group's
	/// map covers far more of it and keys on the same user id, and the API turns an id into a handle.
	/// The union of the two is what a roster can be here, and the warning says how much of the group
	/// it reached.
	async fn members(&mut self, at: &VenueRef) -> Result<Vec<Member>> {
		let (mut roster, total) = self.listed(at).await?;
		let pins = self.pins(at).await?;
		info!("skool `{}`: {} on the member page, {} on the map, of {total}", at.slug, roster.len(), pins.len());

		let mut refused = 0usize;
		for (id, (lat, lon)) in &pins {
			if !roster.contains_key(id) {
				time::sleep(PACE).await;
				// one pin skool will not resolve costs that pin: a member without a handle is one
				// nothing downstream could address anyway, and the rest of the sweep is worth keeping
				match self.user(id).await {
					Ok(member) => {
						roster.insert(id.clone(), member);
					}
					Err(e) => {
						refused += 1;
						warn!("skool `{}`: {id} would not resolve, skipping: {e:#}", at.slug);
						continue;
					}
				}
			}
			let member = roster.get_mut(id).expect("inserted above when absent");
			member.lat = Some(*lat);
			member.lon = Some(*lon);
		}
		if refused > 0 {
			warn!("skool `{}`: {refused} of {} pins would not resolve", at.slug, pins.len());
		}

		if roster.len() < total {
			warn!(
				"skool `{}`: {} of {total} members — the rest carry no map pin, and skool pages its member list nowhere",
				at.slug,
				roster.len()
			);
		}
		Ok(roster.into_values().collect())
	}

	/// The group feed, page by page, with every post's replies under it. `postTrees` is the same array
	/// a profile carries, so the parse is shared — but a feed pages where a profile does not, and the
	/// replies are on no page skool renders at all. One request per post that has any is what makes a
	/// whole-group read expensive, and therefore hand-run.
	async fn posts(&mut self, at: &VenueRef, window: Window, _assets: &Path) -> Result<Page> {
		let slug = at.slug.clone();
		let mut out = Page::default();
		let mut seen: HashSet<String> = HashSet::new();
		//LOOP: bounded by the feed, which is finite and walked from the newest page strictly downwards
		for p in 1.. {
			let payload = self.group_page(&at.slug, &format!("?p={p}")).await?;
			let served = payload
				.pointer("/props/pageProps/postTrees")
				.and_then(|v| v.as_array())
				.ok_or_else(|| eyre!("skool `{}`: no postTrees", at.slug))?
				.clone();
			if served.is_empty() {
				out.exhausted = true;
				break;
			}
			// A pinned post heads the feed *and* keeps its own chronological place, so the same post is
			// served twice — both inside page 1, and again on whichever page its date falls on. Dropping
			// the repeat here rather than downstream is what keeps one post to one reply fetch.
			let mut nodes = Vec::with_capacity(served.len());
			for node in served {
				let id = node
					.pointer("/post/id")
					.and_then(|v| v.as_str())
					.ok_or_else(|| eyre!("a skool postTree without a post id: {node}"))?;
				if seen.insert(id.to_string()) {
					nodes.push(node);
				}
			}

			let page = page_of(&nodes, &window, Kind::Post, Some(&at.slug), |node| {
				// the author rides on the tree node; a post whose author skool withheld is still the group's
				Author::Handle(
					node.pointer("/user/name")
						.or_else(|| node.pointer("/post/user/name"))
						.and_then(|v| v.as_str())
						.unwrap_or(&slug)
						.to_string(),
				)
			})?;
			// the first page's first post is the feed's newest, and the only checkpoint worth keeping
			if let Some(newest) = page.newest {
				out.newest.get_or_insert(newest);
			}
			out.oldest = page.oldest.or(out.oldest);
			// short of the page it was handed means the window stopped it, and nothing under it is wanted
			let stopped = page.items.len() < nodes.len();
			let taken: HashMap<&str, &Item> = page.items.iter().map(|item| (item.id.as_str(), item)).collect();

			let mut replies = Vec::new();
			for node in &nodes {
				let post = node.get("post").ok_or_else(|| eyre!("a skool postTree without a post: {node}"))?;
				let id = post.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool post without an id: {post}"))?;
				// a post the window stopped short of is one whose replies are not being read either
				let Some(item) = taken.get(id) else { continue };
				if post.pointer("/metadata/comments").and_then(|v| v.as_u64()).unwrap_or(0) == 0 {
					continue;
				}
				let group = post.get("groupId").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool post {id} without a groupId"))?;
				let permalink = item.permalink.clone().ok_or_else(|| eyre!("skool post {id} was built without a permalink"))?;
				time::sleep(PACE).await;
				replies.extend(self.replies(id, group, &permalink).await?);
			}
			info!("skool `{}`: page {p}, {} posts, {} replies", at.slug, page.items.len(), replies.len());
			out.items.extend(page.items);
			out.items.extend(replies);
			if stopped {
				break;
			}
			if out.items.len() >= window.limit() {
				warn!("skool `{}`: stopping at {} items, the rest comes on the next read", at.slug, out.items.len());
				break;
			}
			time::sleep(PACE).await;
		}
		// a page is oldest-first and the feed is walked newest-page-first, so neither order survives
		// the concatenation on its own
		out.items.sort_by_key(|item| item.at);
		Ok(out)
	}
}

/// Skool's chat is the one thing here that cannot wait to be asked: a `/ping` is only worth
/// anything while the sender is still at their keyboard. Nothing on skool pushes, so this polls the
/// channel listing — one request, which names every open chat, who is on the other end of it and
/// what they last said. Only a channel whose last message moved costs a second request.
pub struct SkoolDms {
	session: Skool,
	tx: UnboundedSender<DmEvent>,
	/// Last message id seen per channel. Seeded by the first poll and empty before it: a fresh process
	/// has no known gap, and replaying one would re-beep every restart.
	cursors: HashMap<String, String>,
	seeded: bool,
	/// Skool's block page and its cookie rotation both look like a failed poll, so one is not worth
	/// bringing the daemon down over — [`POLL_FAILURES`] of them in a row is.
	failures: usize,
}

impl SkoolDms {
	pub fn try_new(creds: SkoolCredentials, tx: UnboundedSender<DmEvent>) -> Result<Self> {
		Ok(Self {
			session: Skool::try_new(Some(creds))?,
			tx,
			cursors: HashMap::new(),
			seeded: false,
			failures: 0,
		})
	}

	async fn poll(&mut self) -> Result<()> {
		for channel in self.session.chat_channels().await? {
			let id = channel.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a chat channel without an id: {channel}"))?;
			// a channel opened by a `chat-request` nobody has written in yet
			let Some(last) = channel.get("last_message_id").and_then(|v| v.as_str()) else { continue };
			let seen = self.cursors.get(id).cloned();
			if seen.as_deref() == Some(last) {
				continue;
			}
			let them = channel.pointer("/user").ok_or_else(|| eyre!("a chat channel without the other party: {channel}"))?;
			let handle = them.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a chat channel party without a name: {them}"))?;
			let them_id = them.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a chat channel party without an id: {them}"))?;
			if !self.seeded {
				self.cursors.insert(id.to_string(), last.to_string());
				continue;
			}
			let (page, _) = match &seen {
				// one page is what a poll interval can plausibly hold; anything past it walks up on the next
				// poll, since the cursor lands on the newest message this one saw
				Some(seen) => self.session.chat_page(id, Some(seen), Side::After, CHAT_PAGE).await?,
				// skool serves 30 channels and no more, so a channel with no cursor is either one that was
				// just opened or one a new message has just carried back into the listing — its tail is what
				// is new in both cases
				None => self.session.chat_page(id, None, Side::Before, CATCH_UP).await?,
			};
			for message in &page {
				let item = chat_item(message, handle, them_id, id)?;
				if matches!(item.author, Author::Me) {
					continue;
				}
				// a closed receiver is `dms::run` gone, which is the process coming down
				let _ = self.tx.send(DmEvent::Message {
					platform: PLATFORM,
					sender: handle.to_string(),
					text: item.text,
					chat_id: id.to_string(),
					is_dm: true,
					// skool's chat has neither a mention nor a reply
					mentions_me: false,
					is_reply_to_me: false,
				});
			}
			// stamped last, so a refused fetch is retried from the same place rather than skipped over
			let newest = page.last().map(message_id).transpose()?.unwrap_or(last);
			self.cursors.insert(id.to_string(), newest.to_string());
		}
		self.seeded = true;
		Ok(())
	}
}

impl Client for SkoolDms {
	fn surface(&self) -> &'static str {
		SURFACE
	}

	async fn listen(&mut self) -> Result<Infallible, AdapterError> {
		//LOOP: a poller, and the only way out is `POLL_FAILURES` refusals in a row
		loop {
			match self.poll().await {
				Ok(()) => self.failures = 0,
				Err(e) => {
					self.failures += 1;
					warn!("skool dms: poll {} of {POLL_FAILURES} failed: {e:#}", self.failures);
					if self.failures >= POLL_FAILURES {
						return Err(AdapterError::Unhandled {
							surface: SURFACE,
							detail: format!("{POLL_FAILURES} polls in a row refused: {e:#}"),
						});
					}
				}
			}
			time::sleep(POLL).await;
		}
	}
}

/// One course of a group's classroom — a module, in skool's own wording, and a page of its own.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Course {
	/// The 8-hex `name`, which is what `/classroom/<x>` addresses — not the 32-hex id a lesson uses
	pub id: String,
	pub title: String,
	pub permalink: String,
	pub at: Timestamp,
	pub body: String,
	/// In the order skool serves them, which is the order they are meant to be taken in
	pub lessons: Vec<Lesson>,
}

/// One lesson of a group's classroom. Not an [`Item`]: nobody wrote it and nobody replied to it, and
/// the two things it is read *for* — where it sits in the course and what video it plays — are the
/// two an item cannot carry.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Lesson {
	pub id: String,
	/// Every title above it, `/`-joined. A skool classroom is course → lesson here and nests deeper
	/// elsewhere, so the trail is the position and the depth is not.
	pub module: String,
	pub title: String,
	pub permalink: String,
	/// When the lesson last changed. A classroom is a snapshot rather than a feed, so this is the only
	/// time on it worth having — when it was first published answers nothing a re-read asks.
	pub at: Timestamp,
	pub body: String,
	/// Whatever the payload carries, raw: the loom/youtube/vimeo URL somebody pasted, or — for a video
	/// skool hosts itself — a mux playback URL, which is signed, expires within the hour and is served
	/// only under a `Referer: https://www.skool.com/`. `None` for a lesson that is text alone.
	pub video: Option<String>,
	/// The files and links attached beside the body, as the payload's own JSON and not a reading of
	/// it: every classroom seen so far leaves this empty, so its shape is unobserved and a parse here
	/// would be a guess. `None` for the empty list.
	pub resources: Option<String>,
}

/// One course tree, depth-first: every lesson's id, and the trail of titles it hangs under. Only the
/// order and the shape — what a lesson *says* is on its own route and nowhere else.
fn lessons_of(children: Option<&serde_json::Value>, above: &str, out: &mut Vec<(String, String)>) -> Result<()> {
	for child in children.and_then(|v| v.as_array()).into_iter().flatten() {
		let node = child.get("course").ok_or_else(|| eyre!("a skool course tree node without a course: {child}"))?;
		let id = node.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool lesson without an id: {node}"))?;
		let title = node
			.pointer("/metadata/title")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("skool lesson {id} without a title: {node}"))?;
		// a node that holds other nodes is a folder in skool's own UI, and plays nothing
		if child.get("children").and_then(|v| v.as_array()).is_some_and(|nested| !nested.is_empty()) {
			lessons_of(child.get("children"), &format!("{above} / {title}"), out)?;
			continue;
		}
		out.push((id.to_string(), above.to_string()));
	}
	Ok(())
}

/// The same tree again, for the one node a lesson's own route serves whole.
fn node_of(children: Option<&serde_json::Value>, id: &str) -> Option<serde_json::Value> {
	for child in children.and_then(|v| v.as_array()).into_iter().flatten() {
		if child.pointer("/course/id").and_then(|v| v.as_str()) == Some(id) {
			return child.get("course").cloned();
		}
		if let Some(found) = node_of(child.get("children"), id) {
			return Some(found);
		}
	}
	None
}

/// The second half of a pair is the id of a video skool hosts, which only the `video` beside this
/// node resolves.
fn lesson_of(node: &serde_json::Value, slug: &str, course: &str, module: String) -> Result<(Lesson, Option<String>)> {
	let id = node.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool lesson without an id: {node}"))?;
	let title = node
		.pointer("/metadata/title")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("skool lesson {id} without a title: {node}"))?;
	let updated = node
		.get("updatedAt")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("skool lesson {id} without an updatedAt: {node}"))?;
	let metadata = |key: &str| node.pointer(&format!("/metadata/{key}")).and_then(|v| v.as_str()).filter(|v| !v.is_empty());
	// a pasted link outlives a mux token and costs no request, so it wins wherever skool holds both
	let pasted = metadata("videoLink").map(str::to_string);
	let hosted = pasted.is_none().then(|| metadata("videoId").map(str::to_string)).flatten();
	Ok((
		Lesson {
			id: id.to_string(),
			module,
			title: title.to_string(),
			permalink: format!("{BASE}/{slug}/classroom/{course}?md={id}"),
			at: updated.parse().wrap_err("skool timestamps are RFC3339")?,
			body: metadata("desc").map(rich_text).transpose()?.unwrap_or_default(),
			video: pasted,
			resources: metadata("resources").filter(|v| *v != "[]").map(str::to_string),
		},
		hosted,
	))
}

/// Skool's editor writes tiptap JSON behind a `[v2]` marker. Anything without one is the plain text
/// the editor before it left, and is already what it says.
fn rich_text(desc: &str) -> Result<String> {
	let Some(json) = desc.strip_prefix("[v2]") else { return Ok(desc.trim().to_string()) };
	let mut out = String::new();
	flatten(&serde_json::from_str(json).wrap_err("a `[v2]` description is tiptap json")?, &mut out)?;
	Ok(out.trim().to_string())
}

fn flatten(node: &serde_json::Value, out: &mut String) -> Result<()> {
	match node {
		serde_json::Value::Array(list) => list.iter().try_for_each(|node| flatten(node, out)),
		serde_json::Value::Object(map) => {
			let kind = map.get("type").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a tiptap node without a type: {node}"))?;
			match kind {
				"text" => {
					let text = map.get("text").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a tiptap text node without text: {node}"))?;
					out.push_str(text);
					// a link whose text is not its own address is the one place the address is only in the mark
					match map
						.get("marks")
						.and_then(|v| v.as_array())
						.into_iter()
						.flatten()
						.find_map(|mark| mark.pointer("/attrs/href")?.as_str())
					{
						Some(href) if href != text => out.push_str(&format!(" ({href})")),
						_ => (),
					}
				}
				"hardBreak" => out.push('\n'),
				_ => (),
			}
			if let Some(content) = map.get("content") {
				flatten(content, out)?;
			}
			// a list item wraps a paragraph, so both close the same line and only the first of them ends it
			if matches!(kind, "paragraph" | "heading" | "listItem") && !out.ends_with('\n') {
				out.push('\n');
			}
			Ok(())
		}
		_ => Ok(()),
	}
}

/// A video skool hosts is a mux asset, and the page is handed a token for it rather than a URL. The
/// token is short-lived and carries a playback restriction, so what comes out of here plays for
/// about an hour and only under skool's own `Referer`.
fn mux(video: &serde_json::Value) -> Result<String> {
	let status = video.get("status").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a mux video without a status: {video}"))?;
	if status != "ready" {
		bail!("mux says `{status}` for {video}");
	}
	let playback = video
		.get("playbackId")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("a ready mux video without a playbackId: {video}"))?;
	let token = video
		.get("playbackToken")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("a ready mux video without a playbackToken: {video}"))?;
	Ok(format!("https://stream.mux.com/{playback}.m3u8?token={token}"))
}

/// One `postTrees` array, newest-first, turned into a page: a group feed and a profile serve the
/// same nodes, so they take the same parse. `of` is the group whose feed this is, and is what a post
/// that leaves its own group implied is filed under.
///
/// Ids are opaque hex, so the cursor can only be *recognised*, not compared — a post that has already
/// scrolled off the first page is reported again rather than missed.
fn page_of(nodes: &[serde_json::Value], window: &Window, kind: Kind, of: Option<&str>, attribute: impl Fn(&serde_json::Value) -> Author) -> Result<Page> {
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
		// a group's own feed leaves the group implied; a profile carries one per post, from any group
		let group = post.pointer("/group/name").and_then(|v| v.as_str()).or(of);
		let (Some(group), Some(name)) = (group, post.get("name").and_then(|v| v.as_str())) else {
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
			text: mentions(&match body.is_empty() {
				true => title.to_string(),
				false => format!("{title}\n{body}"),
			}),
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

/// One comment tree, depth-first. The API spells in snake_case what the rendered feed spells in
/// camelCase; a reply carries the post's permalink because skool gives it no address of its own.
fn replies_of(children: Option<&serde_json::Value>, permalink: &str, out: &mut Vec<Item>) -> Result<()> {
	for child in children.and_then(|v| v.as_array()).into_iter().flatten() {
		let post = child.get("post").ok_or_else(|| eyre!("a skool comment tree node without a post: {child}"))?;
		let id = post.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool comment without an id: {post}"))?;
		let created = post.get("created_at").and_then(|v| v.as_str()).ok_or_else(|| eyre!("skool comment {id} without a created_at"))?;
		let handle = post
			.pointer("/user/name")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("skool comment {id} without an author: {post}"))?;
		out.push(Item {
			id: id.to_string(),
			source: Source::Skool,
			at: created.parse().wrap_err("skool timestamps are RFC3339")?,
			kind: Kind::Comment,
			author: Author::Handle(handle.to_string()),
			text: mentions(post.pointer("/metadata/content").and_then(|v| v.as_str()).unwrap_or_default().trim()),
			attachments: Vec::new(),
			permalink: Some(permalink.to_string()),
		});
		replies_of(child.get("children"), permalink, out)?;
	}
	Ok(())
}

/// `[@Name](obj://user/<id>)` is how skool encodes a mention, and the id in it addresses nothing
/// outside skool's own client.
fn mentions(text: &str) -> String {
	static MENTION: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[(@[^\]]*)\]\(obj://[^)]*\)").expect("static pattern"));
	MENTION.replace_all(text, "$1").into_owned()
}

/// A user, off an SSR page or off the API. The two spell the same fields in camelCase and in
/// snake_case respectively, which is the whole of the difference between them.
fn member(user: &serde_json::Value) -> Result<Member> {
	let handle = user.get("name").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool member without a name: {user}"))?;
	Ok(Member {
		display: full_name(user).unwrap_or_else(|| handle.to_string()),
		handle: handle.to_string(),
		// `member` is their membership of *this* group; a bare `createdAt` is their account
		joined: user
			.pointer("/member/createdAt")
			.and_then(|v| v.as_str())
			.map(str::parse::<Timestamp>)
			.transpose()
			.wrap_err("skool timestamps are RFC3339")?,
		lat: None,
		lon: None,
		zone: field(user, "timeZone", "time_zone").map(str::to_string),
	})
}

/// Skool keeps the two halves apart and prints neither on its own. `None` when somebody filled in
/// no name at all, which is not the same as their handle.
fn full_name(user: &serde_json::Value) -> Option<String> {
	match (field(user, "firstName", "first_name"), field(user, "lastName", "last_name")) {
		(Some(first), Some(last)) => Some(format!("{first} {last}")),
		(first, last) => first.or(last).map(str::to_string),
	}
}

fn field<'a>(user: &'a serde_json::Value, camel: &str, snake: &str) -> Option<&'a str> {
	[camel, snake]
		.into_iter()
		.find_map(|key| user.get(key).and_then(|v| v.as_str()).map(str::trim).filter(|v| !v.is_empty()))
}

/// Which side of the pivot [`Skool::chat_page`] walks, and the flag skool answers for that side.
#[derive(Clone, Copy)]
enum Side {
	Before,
	After,
}
impl Side {
	fn as_str(self) -> &'static str {
		match self {
			Self::Before => "before",
			Self::After => "after",
		}
	}

	fn more(self) -> &'static str {
		match self {
			Self::Before => "has_more_before",
			Self::After => "has_more_after",
		}
	}
}

fn message_id(message: &serde_json::Value) -> Result<&str> {
	message.get("id").and_then(|v| v.as_str()).ok_or_else(|| eyre!("a skool chat message without an id: {message}"))
}

/// `metadata.src` is who sent it, so the handle's own user id is what tells the two apart. Skool
/// gives a message no address of its own; the channel is as close as one gets.
fn chat_item(message: &serde_json::Value, handle: &str, user: &str, channel: &str) -> Result<Item> {
	let id = message_id(message)?;
	let created = message
		.get("created_at")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("skool chat message {id} without a created_at"))?;
	let src = message
		.pointer("/metadata/src")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("skool chat message {id} without a src: {message}"))?;
	let content = message
		.pointer("/metadata/content")
		.and_then(|v| v.as_str())
		.ok_or_else(|| eyre!("skool chat message {id} without content: {message}"))?;
	Ok(Item {
		id: id.to_string(),
		source: Source::Skool,
		at: created.parse().wrap_err("skool timestamps are RFC3339")?,
		kind: Kind::Direct,
		author: match src == user {
			true => Author::Handle(handle.to_string()),
			false => Author::Me,
		},
		text: mentions(content.trim()),
		attachments: Vec::new(),
		permalink: Some(format!("{BASE}/chat?c={channel}")),
	})
}

/// One member's position on a group's map: `u` is their user id, `p` is `[lat, lon]`.
#[derive(Deserialize)]
struct Pin {
	u: String,
	p: [f64; 2],
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

		let all = page_of(&nodes, &Window::above(None), Kind::Post, None, |_| Author::Me).unwrap();
		assert_eq!(all.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["a", "b", "c"], "oldest-first");
		assert_eq!(all.newest.as_deref(), Some("c"));
		assert_eq!(all.items[0].permalink.as_deref(), Some("https://www.skool.com/g/a-post"));

		let since = page_of(&nodes, &Window::above(Some("b".to_string())), Kind::Post, None, |_| Author::Me).unwrap();
		assert_eq!(since.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["c"]);

		let dated = page_of(&nodes, &Window::since("2026-03-02T00:00:00Z".parse().unwrap()), Kind::Post, None, |_| Author::Me).unwrap();
		assert_eq!(dated.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(), ["b", "c"]);
	}

	/// A lesson body is tiptap json, and the only part of it worth keeping is text — except for a link
	/// whose text is not its own address, which is the one place a URL exists only in a mark.
	#[test]
	fn a_lesson_body_is_its_links_and_its_text() {
		let link = |href: &str, text: &str| serde_json::json!({"type": "text", "text": text, "marks": [{"type": "link", "attrs": {"class": "link", "href": href}}]});
		let desc = serde_json::json!([
			{"type": "paragraph", "content": [link("https://www.loom.com/share/0be", "https://www.loom.com/share/0be")]},
			{"type": "bulletList", "content": [{"type": "listItem", "content": [{"type": "paragraph", "content": [
				{"type": "text", "text": "if using an iphone, use "},
				link("http://getghostme.com", "getghostme.com"),
				{"type": "hardBreak"},
				{"type": "text", "text": "then "},
				link("https://player.vimeo.com/video/1079014922", "this one")
			]}]}]}
		]);
		assert_eq!(
			rich_text(&format!("[v2]{desc}")).unwrap(),
			"https://www.loom.com/share/0be\nif using an iphone, use getghostme.com (http://getghostme.com)\nthen this one (https://player.vimeo.com/video/1079014922)"
		);
		// skool stores an untouched description as an empty paragraph rather than dropping the field
		assert_eq!(rich_text(r#"[v2][{"type":"paragraph"}]"#).unwrap(), "");
		assert_eq!(rich_text(" a course blurb is plain ").unwrap(), "a course blurb is plain");
	}

	/// The name skool prints is two fields it never joins itself — the gap that had a person file
	/// named by hand.
	#[test]
	fn a_display_name_is_two_fields() {
		let user = serde_json::json!({"firstName": "Lory", "lastName": "Bellardant"});
		assert_eq!(full_name(&user).as_deref(), Some("Lory Bellardant"));
		assert_eq!(full_name(&serde_json::json!({"firstName": "Lory"})).as_deref(), Some("Lory"));
		assert_eq!(full_name(&serde_json::json!({"firstName": ""})), None);
	}
}
