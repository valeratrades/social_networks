//! Skool publishes no API. Every page is Next.js SSR, so the whole payload sits in `__NEXT_DATA__`
//! and a plain GET is a complete read. Only `/auth/*` is gated behind an AWS-WAF JS challenge, so
//! cookies can be minted by a browser and by nothing else — but the browser stays off the read path
//! and runs about once per token rotation.
//!
//! Writes have nowhere to go but the undocumented REST API its own web client talks to.
//!
//! Everything a profile carries is public. The cookie only adds what membership can see, so a
//! [`Skool`] built without credentials is a working reader rather than a degraded one.

use std::{
	io::Write as _,
	os::unix::fs::OpenOptionsExt as _,
	path::PathBuf,
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
use regex::Regex;
use tracing::{info, instrument};

const BASE: &str = "https://www.skool.com";
/// Everything the SSR payload cannot say, and every write. Cookie-authenticated, same as [`BASE`].
const API: &str = "https://api.skool.com";
/// Long enough for a slow WAF challenge, short enough that a wedged browser does not hold a daemon.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";
#[derive(Clone, Debug)]
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

	/// Skool's chat lives behind the one thing its SSR pages are not: a REST API at [`API`]. A DM is
	/// a channel away, and `chat-request` is what the web client's "Chat" button calls every time —
	/// it hands back the channel already open with that person rather than a second one.
	pub async fn dm(&mut self, handle: &str, text: &str) -> Result<()> {
		let handle = handle.trim_start_matches('@');
		let profile = self.page(&format!("/@{handle}")).await?;
		let user = profile
			.pointer("/props/pageProps/currentUser/id")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("no such skool handle: `{handle}`"))?
			.to_string();

		let requested = self.api(&format!("/users/{user}/chat-request"), &[], None).await?;
		let requested: serde_json::Value = serde_json::from_str(&requested).wrap_err_with(|| format!("a chat request for `{handle}` answered {requested}"))?;
		let channel = requested
			.pointer("/channel/id")
			.and_then(|v| v.as_str())
			.ok_or_else(|| eyre!("a chat request for `{handle}` came back without a channel: {requested}"))?
			.to_string();

		// `ct` is the client the message was typed in; the web chat calls itself `wdc`
		self.api(&format!("/channels/{channel}/messages"), &[("ct", "wdc")], Some(serde_json::json!({ "content": text })))
			.await
			.map(|_| ())
	}

	/// Unlike the SSR pages, which answer a dead session by serving the signed-out view, the API says
	/// 401 — so that, rather than the payload, is what rotation hangs off here.
	async fn api(&mut self, path: &str, query: &[(&str, &str)], body: Option<serde_json::Value>) -> Result<String> {
		let mut response = self.post(path, query, body.as_ref()).await?;
		if response.status() == reqwest::StatusCode::UNAUTHORIZED && self.creds.is_some() {
			self.refresh().await?;
			response = self.post(path, query, body.as_ref()).await?;
		}
		let status = response.status();
		let payload = response.text().await?;
		if !status.is_success() {
			bail!("POST {path} answered {status}: {payload}");
		}
		Ok(payload)
	}

	async fn post(&self, path: &str, query: &[(&str, &str)], body: Option<&serde_json::Value>) -> Result<reqwest::Response> {
		let mut request = self.http.post(format!("{API}{path}")).query(query);
		if let Some(cookie) = &self.cookie {
			request = request.header(reqwest::header::COOKIE, cookie);
		}
		// a bodyless POST still has to declare itself json, or the API answers 415
		request = match body {
			Some(body) => request.json(body),
			None => request.header(reqwest::header::CONTENT_TYPE, "application/json"),
		};
		request.send().await.wrap_err_with(|| format!("POST {path}"))
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

/// A session cookie is a bearer credential, so the file it lives in is `0600`.
#[derive(serde::Deserialize, serde::Serialize)]
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
	let url = loop {
		// `None` is a frame that has not committed a navigation yet, which is not somewhere to be
		match page.url().await? {
			Some(url) if !url.contains("/login") => break url,
			url =>
				if Instant::now() >= deadline {
					bail!("still on {url:?} {LOGIN_TIMEOUT:?} after submitting the login form");
				},
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
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
}
