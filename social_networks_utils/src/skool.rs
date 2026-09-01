//! Skool has no API. Every page is Next.js SSR, so the whole payload sits in `__NEXT_DATA__` and a
//! plain GET is a complete read. Only `/auth/*` is gated behind an AWS-WAF JS challenge, so cookies
//! can be minted by a browser and by nothing else — but the browser stays off the read path and runs
//! about once per token rotation.
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
use color_eyre::eyre::{Result, WrapErr, eyre};
use futures::{
	StreamExt as _,
	future::{Either, select},
};
use regex::Regex;
use tracing::{info, instrument};

const BASE: &str = "https://www.skool.com";
/// Long enough for a slow WAF challenge, short enough that a wedged browser does not hold a daemon.
const LOGIN_TIMEOUT: Duration = Duration::from_secs(90);

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
	pub fn new(creds: Option<SkoolCredentials>) -> Result<Self> {
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
		let creds = self.creds.clone().expect("`page` only refreshes when credentials are present");
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
					return Err(eyre!("still on {url:?} {LOGIN_TIMEOUT:?} after submitting the login form"));
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
		return Err(eyre!("login navigated to {url} but left no skool.com cookies"));
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

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

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
