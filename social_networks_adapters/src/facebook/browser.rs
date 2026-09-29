//! A logged-in chrome, driven through `browser_manipulation`, whose driver never sends `Runtime.enable`
//! and launches without automation flags: see `docs/facebook/session_drop.md`.

use std::{
	io::Write as _,
	path::{Path, PathBuf},
	pin::pin,
	sync::{Arc, Mutex},
	time::Duration,
};

use browser_manipulation::{Artifacts, Browser, Launch, Noise, Request};
use color_eyre::{
	Report,
	eyre::{Result, WrapErr, bail, ensure},
};
use futures::{
	StreamExt as _,
	future::{Either, select},
};
use jiff::{SignedDuration, Timestamp};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader};

use super::sway::{self, Window};
use crate::browser_failure;

const GRAPHQL: &str = "/api/graphql/";

pub(super) struct Tab<'a> {
	page: browser_manipulation::Tab<'a, Noise>,
	login: Option<Login>,
	/// no human can see it: nothing to wait for
	headless: bool,
	/// `sessions.toml` of the account
	drops: PathBuf,
	/// the attached window, once a wheel went unacked because nobody could see it
	window: &'a Mutex<Option<Window>>,
}
impl Tab<'_> {
	/// Navigates, then refuses to go on from anything but a logged-in, English, unblocked page: every
	/// request past a checkpoint or a block digs the account deeper. A logged-out landing waits for the
	/// human to log in, then navigates again.
	pub(super) async fn goto(&mut self, url: &str) -> Result<()> {
		loop {
			let landed = self.navigate(url).await?;
			if landed.contains("/checkpoint") {
				bail!("facebook put the account through a checkpoint ({landed}); resolve it by hand before running again");
			}
			let Some(logged_in_at) = self.logged_in_at().await? else {
				self.relogin(&landed).await?;
				continue;
			};
			if landed.contains("/login") {
				bail!("landed on {landed} while holding a `c_user` cookie; look at the scraped tab");
			}
			let login = self.login.get_or_insert_with(|| Login {
				at: logged_in_at,
				page_views: 0,
				scrolls: 0,
				last_url: String::new(),
			});
			login.page_views += 1;
			login.last_url = landed.clone();
			let lang: String = self.eval("document.documentElement.lang").await?;
			if !lang.starts_with("en") {
				bail!("facebook is serving `{lang}`; switch the account's language to English, which is what the extraction reads");
			}
			let text = self.eval::<String>("document.body.innerText").await?.to_lowercase().replace('’', "'");
			for marker in ["you're temporarily blocked", "you can't use this feature right now", "your account has been locked"] {
				if text.contains(marker) {
					bail!("facebook says \"{marker}\" on {landed}; stop and wait it out");
				}
			}
			return Ok(());
		}
	}

	/// Where the tab ended up.
	async fn navigate(&mut self, url: &str) -> Result<String> {
		self.page.goto(url).await.map_err(browser_failure)?;
		Ok(self.page.url())
	}

	/// From the `c_user` cookie, which facebook sets to expire a year after the login.
	async fn logged_in_at(&self) -> Result<Option<Timestamp>> {
		let cookies = self.page.cookies().await.map_err(browser_failure)?;
		let Some(c) = cookies.iter().find(|c| c.name == "c_user") else {
			return Ok(None);
		};
		let expires = Timestamp::from_second(c.expires as i64)?;
		Ok(Some(expires.checked_sub(SignedDuration::from_hours(24 * 365))?))
	}

	/// Logs why the session ended, then waits for the human to log in again in this tab. Credentials are never ours to type.
	async fn relogin(&mut self, landed: &str) -> Result<()> {
		if let Some(login) = self.login.take() {
			let dropped_at = Timestamp::now();
			let line = SessionEnd {
				logged_in_at: login.at,
				lifetime: dropped_at.duration_since(login.at),
				page_views: login.page_views,
				scrolls: login.scrolls,
				last_url: login.last_url,
			};
			let path = &self.drops;
			let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
			writeln!(f, "\"{dropped_at}\" = {}", toml::Value::try_from(&line)?).wrap_err_with(|| format!("failed to write {}", path.display()))?;
			eprintln!("facebook session ended after {:#} (landed on {landed}); logged to {}", line.lifetime, path.display());
		}
		if self.headless {
			bail!("the account is logged out (landed on {landed}); log it in with `recon facebook-login`");
		}
		if !landed.contains("/login") {
			self.navigate("https://www.facebook.com/login").await?;
		}
		notify("facebook: session ended — log in in the scraped tab")?;
		eprintln!("waiting for a facebook login in the scraped tab");
		while self.logged_in_at().await?.is_none() {
			tokio::time::sleep(Duration::from_secs(5)).await;
			self.eval::<u8>("1").await.wrap_err("the scraped tab was closed while waiting for the login")?;
		}
		Ok(())
	}

	/// `expr` must evaluate to something JSON-serializable. Read-only by convention: page state is the site's.
	async fn eval<T: DeserializeOwned>(&mut self, expr: &str) -> Result<T> {
		self.page.eval(expr, ()).await.map_err(browser_failure)
	}

	/// The JSON the page embeds, one `application/json` script each: facebook's first render.
	pub(super) async fn scripts(&mut self) -> Result<Vec<String>> {
		self.eval(r#"[...document.querySelectorAll('script[type="application/json"]')].map(s => s.textContent)"#).await
	}

	/// One wheel gesture of 3–8 notches over the viewport's middle third, then feeds each `/api/graphql/`
	/// answer to `absorb` until it reports progress or 10 s pass; whether it did.
	///
	/// With `from` set, the pagination request the gesture sets off asks for the page past that cursor
	/// instead of the next one, and `from` is taken: the page's own request, sent by the page, differing
	/// only in where it resumes.
	pub(super) async fn scroll(&mut self, from: &mut Option<String>, mut absorb: impl FnMut(&str) -> Result<bool>) -> Result<bool> {
		let routed = from.is_some();
		let resume = Arc::new(Mutex::new(Resume { from: from.take(), failed: None }));
		if routed {
			let resume = resume.clone();
			let rewrite = move |request: &Request| {
				let mut r = resume.lock().expect("never held across a panic");
				let page = r.from.as_deref()?;
				match self::resume(request, page) {
					Ok(Some(body)) => {
						r.from = None;
						Some(body)
					}
					Ok(None) => None,
					Err(e) => {
						r.failed.get_or_insert(e);
						None
					}
				}
			};
			self.page.route(GRAPHQL, rewrite).await.map_err(browser_failure)?;
		}
		let r = self.gesture(&resume, &mut absorb).await;
		if routed {
			self.page.unroute(GRAPHQL).await.map_err(browser_failure)?;
		}
		let Resume { from: rest, failed } = std::mem::replace(&mut *resume.lock().expect("never held across a panic"), Resume { from: None, failed: None });
		*from = rest;
		if let Some(e) = failed {
			return Err(e.wrap_err("rewriting the cursor of a pagination request"));
		}
		r
	}

	async fn gesture(&mut self, resume: &Mutex<Resume>, absorb: &mut impl FnMut(&str) -> Result<bool>) -> Result<bool> {
		let mut answers = pin!(self.page.responses(GRAPHQL).await.map_err(browser_failure)?);
		let title: String = self.eval("document.title").await?;
		self.wheel(&title).await?;
		if let Some(login) = &mut self.login {
			login.scrolls += 1;
		}
		let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
		while let Ok(next) = tokio::time::timeout_at(deadline, answers.next()).await {
			let answer = next.expect("the tab outlives its listeners").map_err(browser_failure)?;
			let body = std::str::from_utf8(&answer.body).wrap_err_with(|| format!("{} answered with something other than text", answer.url))?;
			if absorb(body)? {
				ensure!(
					resume.lock().expect("never held across a panic").from.is_none(),
					"the page paged on without a request `resume` recognised, so the walk would have gone on from its first page"
				);
				return Ok(true);
			}
		}
		Ok(false)
	}

	/// Real wheel events: `window.scrollTo` does not trigger facebook's pagination. Unacked, the window
	/// titled `title` is taken to be hidden and parked where it renders.
	async fn wheel(&mut self, title: &str) -> Result<()> {
		let (headless, window) = (self.headless, self.window);
		let dy = f64::from(rand::random_range(3..=8u32)) * 110.;
		let mut scroll = pin!(self.page.scroll(None, dy));
		match tokio::time::timeout(Duration::from_secs(10), &mut scroll).await {
			Ok(r) => r.map_err(browser_failure),
			Err(_) if headless => bail!("headless chrome did not ack a wheel event in 10s"),
			Err(_) => {
				{
					let mut window = window.lock().expect("never held across a panic");
					let window = match &mut *window {
						Some(w) => w,
						None => window.insert(Window::find(title)?),
					};
					ensure!(!window.parked(), "chrome did not ack a wheel event in 10s with its window on a headless output");
					window.park()?;
				}
				tokio::time::timeout(Duration::from_secs(10), &mut scroll)
					.await
					.wrap_err("chrome did not ack a wheel event in 10s after its window moved to a headless output")?
					.map_err(browser_failure)
			}
		}
	}
}

/// Runs `work` in the one facebook tab of the chrome debuggable on `port` whose profile is logged in
/// as `user_id`. Non-default profiles are not CDP browser contexts, so the tab is taken over rather
/// than a new one opened next to it. `dir` is the session's state.
pub(super) async fn attach<T>(port: u16, user_id: &str, dir: &Path, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	let launch = Launch::Attach {
		cdp: format!("http://127.0.0.1:{port}"),
	};
	let browser = Browser::launch(launch, motion(), Some(artifacts(dir)))
		.await
		.map_err(browser_failure)
		.wrap_err_with(|| format!("no chrome answering CDP on port {port}"))?;
	let r = async {
		let page = ours(&browser, user_id).await?;
		run(page, false, dir, work).await
	}
	.await;
	closed(r, browser.close().await)
}

/// Runs `work` in a new tab of our own chrome on `profile`, which is started for it and closed after.
pub(super) async fn launch<T>(chrome: &Path, profile: &Path, headless: bool, dir: &Path, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	let launch = Launch::Owned {
		profile: profile.to_path_buf(),
		executable: chrome.to_path_buf(),
		headless,
		viewport: None,
	};
	let browser = Browser::launch(launch, motion(), Some(artifacts(dir))).await.map_err(browser_failure)?;
	let r = async {
		let page = browser.tab().await.map_err(browser_failure)?;
		run(page, headless, dir, work).await
	}
	.await;
	closed(r, browser.close().await)
}

/// The facebook tab whose `c_user` is `user_id`; exactly one.
async fn ours<'b>(browser: &'b Browser<Noise>, user_id: &str) -> Result<browser_manipulation::Tab<'b, Noise>> {
	let mut ours = Vec::new();
	let mut others = Vec::new();
	for page in browser.tabs().into_iter().filter(|t| t.url().starts_with("https://www.facebook.com/")) {
		let c_user = page.cookies().await.map_err(browser_failure)?.into_iter().find(|c| c.name == "c_user").map(|c| c.value);
		match c_user.as_deref() == Some(user_id) {
			true => ours.push(page),
			false => others.push((c_user, page.url())),
		}
	}
	match ours.len() {
		1 => Ok(ours.pop().expect("just counted")),
		0 => bail!("no facebook tab logged in as {user_id}; open one in that account's chrome profile. Other facebook tabs (c_user, url): {others:?}"),
		n => bail!("{n} facebook tabs logged in as {user_id}; keep one: {:?}", ours.iter().map(|t| t.url()).collect::<Vec<_>>()),
	}
}

async fn run<T>(page: browser_manipulation::Tab<'_, Noise>, headless: bool, dir: &Path, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	page.set_timeout(Duration::from_secs(60)).await; // above the 10 s a wheel is given before the window is parked
	let window = Mutex::new(None);
	let run = pin!(async {
		let mut tab = Tab {
			page,
			login: None,
			headless,
			drops: dir.join("sessions.toml"),
			window: &window,
		};
		work(&mut tab).await
	});
	// SIGINT ends the work here rather than the process, so the window goes back and a launched chrome closes
	let interrupted = pin!(async {
		tokio::signal::ctrl_c().await?;
		bail!("interrupted")
	});
	// the window follows the user home; nobody sees a headless chrome, so there is nothing to follow
	let follow = pin!(async {
		if headless {
			return std::future::pending().await;
		}
		let mut sway = sway::subscribe()?;
		let mut events = BufReader::new(sway.stdout.take().expect("piped")).lines();
		while let Some(line) = events.next_line().await? {
			if let Some(w) = window.lock().expect("never held across a panic").as_mut() {
				w.event(&line)?;
			}
		}
		bail!("sway's event stream ended")
	});
	let r = match select(select(run, interrupted), follow).await {
		Either::Left((r, _)) => r.factor_first().0,
		Either::Right((e, _)) => e,
	};
	let window = window.lock().expect("never held across a panic").take();
	if let Some(mut w) = window {
		w.home()?;
	}
	r
}

/// `r`, unless it succeeded and the browser then failed to close.
fn closed<T>(r: Result<T>, close: Result<(), browser_manipulation::Error>) -> Result<T> {
	match (r, close) {
		(r, Ok(())) => r,
		(Ok(_), Err(e)) => Err(browser_failure(e)),
		(Err(r), Err(e)) => Err(r.wrap_err(format!("and closing the browser failed too: {e}"))),
	}
}

/// Pointer and wheel shaped like a hand. Nothing is typed.
fn motion() -> Noise {
	Noise::builder()
		.dwell(Duration::from_millis(250))
		.dwell_spread(0.5)
		.speed(1200.)
		.overshoot(0.15)
		.jitter(1.)
		.key_gap(Duration::from_millis(120))
		.key_spread(0.4)
		.typo(0.)
		.notch(100.0..=120.)
		.notch_gap(Duration::from_millis(8)..=Duration::from_millis(40))
		.back(1. / 15.)
		.back_notches(1..=2)
		.seed(rand::random())
		.build()
}

fn artifacts(dir: &Path) -> Artifacts {
	Artifacts {
		dir: dir.join("browser_captures"),
		retention: Duration::from_secs(14 * 24 * 3600),
	}
}

struct Resume {
	from: Option<String>,
	failed: Option<Report>,
}

/// `request`'s form body with its `cursor` variable set to `page`, when it is a search that pages
/// with one: facebook's pagination queries carry the cursor past the last page shown as a variable.
fn resume(request: &Request, page: &str) -> Result<Option<Vec<u8>>> {
	let Some(body) = &request.body else { return Ok(None) };
	let mut form = reqwest::Url::parse("http://form/").expect("a constant base");
	form.set_query(Some(std::str::from_utf8(body).wrap_err_with(|| format!("{} posted a body that is not form text", request.url))?));
	let mut pairs: Vec<(String, String)> = form.query_pairs().into_owned().collect();
	if !pairs.iter().any(|(k, v)| k == "fb_api_req_friendly_name" && v.starts_with("Search")) {
		return Ok(None);
	}
	let Some((_, variables)) = pairs.iter_mut().find(|(k, _)| k == "variables") else {
		return Ok(None);
	};
	let mut parsed: Value = serde_json::from_str(variables).wrap_err("graphql `variables` is not JSON")?;
	let Some(cursor) = parsed.get_mut("cursor").filter(|c| c.is_string()) else { return Ok(None) };
	*cursor = page.into();
	*variables = parsed.to_string();
	form.query_pairs_mut().clear().extend_pairs(pairs);
	Ok(Some(form.query().expect("just set").as_bytes().to_vec()))
}

/// What happened to the facebook session since we last saw it logged in; one line per drop in `sessions.toml`.
#[derive(Serialize)]
struct SessionEnd {
	logged_in_at: Timestamp,
	lifetime: SignedDuration,
	page_views: u32,
	scrolls: u32,
	last_url: String,
}

struct Login {
	at: Timestamp,
	page_views: u32,
	scrolls: u32,
	last_url: String,
}

fn notify(msg: &str) -> Result<()> {
	let status = std::process::Command::new("notify-send").arg(msg).status().wrap_err("notify-send")?;
	assert!(status.success(), "notify-send failed: {status}");
	Ok(())
}
