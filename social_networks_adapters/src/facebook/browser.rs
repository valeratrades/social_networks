//! A logged-in chromium, driven over a minimal CDP client that never sends `Runtime.enable` and
//! launches without automation flags: see `docs/facebook/session_drop.md`.

use std::{
	collections::{HashMap, HashSet},
	io::Write as _,
	path::{Path, PathBuf},
	pin::pin,
	sync::{
		Mutex,
		atomic::{AtomicU64, Ordering},
	},
	time::Duration,
};

use base64::Engine as _;
use chromiumoxide::{
	cdp::{
		browser_protocol::{
			browser, emulation, fetch, input,
			network::{self, GetResponseBodyParams},
			page::{self, NavigateParams},
			target::{AttachToTargetParams, CreateTargetParams, DetachFromTargetParams, GetTargetsParams, TargetId},
		},
		js_protocol::runtime::EvaluateParams,
	},
	types::{Command, MethodType},
};
use color_eyre::eyre::{Result, WrapErr, bail, ensure, eyre};
use futures::{
	SinkExt, StreamExt,
	future::{Either, select},
	lock::Mutex as AsyncMutex,
	stream::{SplitSink, SplitStream},
};
use jiff::{SignedDuration, Timestamp};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;
use tokio::{
	io::{AsyncBufReadExt as _, BufReader},
	net::TcpStream,
	sync::{mpsc, oneshot},
};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, tungstenite::Message};

use super::sway::{self, Window};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

pub(super) struct Tab<'a> {
	cdp: &'a Cdp,
	session: String,
	events: mpsc::UnboundedReceiver<Event>,
	login: Option<Login>,
	/// no human can see it: nothing to wait for
	headless: bool,
	/// `sessions.toml` of the account
	drops: PathBuf,
	/// the attached window, once a wheel went unacked because nobody could see it
	window: &'a Mutex<Option<Window>>,
}
impl Tab<'_> {
	async fn call<C: Command>(&self, cmd: C) -> Result<C::Response> {
		self.cdp.call(Some(&self.session), cmd).await
	}

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
		while self.events.try_recv().is_ok() {}
		let nav = self.call(NavigateParams::new(url)).await?;
		if let Some(e) = nav.error_text {
			bail!("navigating to {url}: {e}");
		}
		tokio::time::timeout(Duration::from_secs(60), async {
			while let Some(e) = self.events.recv().await {
				if e.parse::<page::EventLoadEventFired>().is_some() {
					return;
				}
			}
		})
		.await
		.wrap_err_with(|| format!("{url} did not finish loading in 60s"))?;
		self.eval("location.href").await
	}

	/// From the `c_user` cookie, which facebook sets to expire a year after the login.
	async fn logged_in_at(&self) -> Result<Option<Timestamp>> {
		let params = network::GetCookiesParams {
			urls: Some(vec!["https://www.facebook.com/".into()]),
		};
		let cookies = self.call(params).await?.cookies;
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
	async fn eval<T: DeserializeOwned>(&self, expr: &str) -> Result<T> {
		let mut params = EvaluateParams::new(expr);
		params.return_by_value = Some(true);
		let r = self.call(params).await?;
		if let Some(e) = r.exception_details {
			bail!("`{expr}` threw: {}", e.exception.and_then(|x| x.description).unwrap_or(e.text));
		}
		serde_json::from_value(r.result.value.ok_or_else(|| eyre!("`{expr}` returned nothing serializable"))?).wrap_err_with(|| format!("`{expr}` returned an unexpected shape"))
	}

	/// The JSON the page embeds, one `application/json` script each: facebook's first render.
	pub(super) async fn scripts(&self) -> Result<Vec<String>> {
		self.eval(r#"[...document.querySelectorAll('script[type="application/json"]')].map(s => s.textContent)"#).await
	}

	/// One wheel gesture, then feeds each `/api/graphql/` answer to `absorb` until it reports progress
	/// or 10 s pass; whether it did. A gesture is a few notches from one spot in the middle third of
	/// the viewport, and now and then a notch or two back up.
	///
	/// With `from` set, the pagination request the gesture sets off asks for the page past that cursor
	/// instead of the next one, and `from` is taken: the page's own request, sent by the page, differing
	/// only in where it resumes.
	pub(super) async fn scroll(&mut self, from: &mut Option<String>, mut absorb: impl FnMut(&str) -> Result<bool>) -> Result<bool> {
		if from.is_some() {
			let pattern = fetch::RequestPattern {
				url_pattern: Some("*/api/graphql/*".into()),
				resource_type: None,
				request_stage: Some(fetch::RequestStage::Request),
			};
			self.call(fetch::EnableParams {
				patterns: Some(vec![pattern]),
				handle_auth_requests: None,
			})
			.await?;
		}
		let r = self.gesture(from, &mut absorb).await;
		if from.is_some() {
			self.call(fetch::DisableParams::default()).await?;
		}
		r
	}

	async fn gesture(&mut self, from: &mut Option<String>, absorb: &mut impl FnMut(&str) -> Result<bool>) -> Result<bool> {
		let (w, h): (f64, f64) = self.eval("[innerWidth, innerHeight]").await?;
		let at = (w * rand::random_range(1. / 3. ..2. / 3.), h * rand::random_range(1. / 3. ..2. / 3.));
		let down = rand::random_range(3..=8);
		let up = match rand::random_ratio(1, 15) {
			true => rand::random_range(1..=2),
			false => 0,
		};
		for tick in 0..down + up {
			if tick > 0 {
				tokio::time::sleep(Duration::from_millis(rand::random_range(8..=40))).await;
			}
			let notch = rand::random_range(100.0..=120.0);
			self.wheel(at, if tick < down { notch } else { -notch }).await?;
		}
		if let Some(login) = &mut self.login {
			login.scrolls += 1;
		}
		let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
		let mut graphql = HashSet::new();
		while let Ok(e) = tokio::time::timeout_at(deadline, self.events.recv()).await {
			let e = e.expect("the driver outlives the tab");
			if let Some(paused) = e.parse::<fetch::EventRequestPaused>() {
				let mut resumed = fetch::ContinueRequestParams::new(paused.request_id);
				if let Some(page) = from.as_deref()
					&& let Some(body) = resume(&paused.request, page)?
				{
					resumed.post_data = Some(base64::engine::general_purpose::STANDARD.encode(body).into());
					*from = None;
				}
				self.call(resumed).await?;
			} else if let Some(r) = e.parse::<network::EventResponseReceived>() {
				if r.response.url.contains("/api/graphql/") {
					graphql.insert(r.request_id);
				}
			} else if let Some(f) = e.parse::<network::EventLoadingFinished>()
				&& graphql.remove(&f.request_id)
			{
				let r = self.call(GetResponseBodyParams::new(f.request_id)).await?;
				assert!(!r.base64_encoded, "graphql answers are text");
				if absorb(&r.body)? {
					ensure!(
						from.is_none(),
						"the page paged on without a request `resume` recognised, so the walk would have gone on from its first page"
					);
					return Ok(true);
				}
			}
		}
		Ok(false)
	}

	/// A real wheel event: `window.scrollTo` does not trigger facebook's pagination. Unacked, the window
	/// is taken to be hidden and parked where it renders.
	async fn wheel(&mut self, (x, y): (f64, f64), delta_y: f64) -> Result<()> {
		let mut params = input::DispatchMouseEventParams::new(input::DispatchMouseEventType::MouseWheel, x, y);
		params.delta_x = Some(0.);
		params.delta_y = Some(delta_y);
		{
			let mut ack = pin!(self.call(params));
			match tokio::time::timeout(Duration::from_secs(10), &mut ack).await {
				Ok(r) => r?,
				Err(_) if self.headless => bail!("headless chrome did not ack a wheel event in 10s"),
				Err(_) => {
					let title: String = self.eval("document.title").await?;
					{
						let mut window = self.window.lock().expect("never held across a panic");
						let window = match &mut *window {
							Some(w) => w,
							None => window.insert(Window::find(&title)?),
						};
						ensure!(!window.parked(), "chrome did not ack a wheel event in 10s with its window on a headless output");
						window.park()?;
					}
					tokio::time::timeout(Duration::from_secs(10), &mut ack)
						.await
						.wrap_err("chrome did not ack a wheel event in 10s after its window moved to a headless output")??
				}
			};
		}
		Ok(())
	}
}

/// Runs `work` in the one facebook tab of the chromium debuggable on `port` whose profile is logged
/// in as `user_id`. Non-default profiles are not CDP browser contexts, so the tab is taken over
/// rather than a new one opened next to it. Session drops are appended to `drops`.
pub(super) async fn attach<T>(port: u16, user_id: &str, drops: &Path, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	let version: Value = reqwest::get(format!("http://127.0.0.1:{port}/json/version"))
		.await
		.wrap_err_with(|| format!("no chromium answering CDP on port {port}"))?
		.json()
		.await?;
	let ws_url = version["webSocketDebuggerUrl"]
		.as_str()
		.ok_or_else(|| eyre!("/json/version without a webSocketDebuggerUrl: {version}"))?;
	let ws = tokio_tungstenite::connect_async(ws_url).await?.0;
	let target = async |cdp: &Cdp| -> Result<String> {
		let targets = cdp.call(None, GetTargetsParams::default()).await?.target_infos;
		let mut ours = Vec::new();
		let mut others = Vec::new();
		for t in targets.into_iter().filter(|t| t.r#type == "page" && t.url.starts_with("https://www.facebook.com/")) {
			let session = cdp.attach(t.target_id).await?;
			let params = network::GetCookiesParams {
				urls: Some(vec!["https://www.facebook.com/".into()]),
			};
			let c_user = cdp.call(Some(&session), params).await?.cookies.into_iter().find(|c| c.name == "c_user").map(|c| c.value);
			match c_user.as_deref() == Some(user_id) {
				true => ours.push((session, t.url)),
				false => {
					cdp.call(None, DetachFromTargetParams { session_id: Some(session.into()) }).await?;
					others.push((c_user, t.url));
				}
			}
		}
		match &ours[..] {
			[(session, _)] => Ok(session.clone()),
			[] => bail!("no facebook tab logged in as {user_id}; open one in that account's chrome profile. Other facebook tabs (c_user, url): {others:?}"),
			many => bail!(
				"{} facebook tabs logged in as {user_id}; keep one: {:?}",
				many.len(),
				many.iter().map(|(_, url)| url).collect::<Vec<_>>()
			),
		}
	};
	run(ws, false, false, drops, target, work).await
}
/// Runs `work` in a new tab of our own chrome on `profile`, which is started for it and closed after.
pub(super) async fn launch<T>(chrome: &Path, profile: &Path, headless: bool, drops: &Path, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	let ws = spawn(chrome, profile, headless).await?;
	let target = async |cdp: &Cdp| cdp.attach(cdp.call(None, CreateTargetParams::new("about:blank")).await?.target_id).await;
	run(ws, headless, true, drops, target, work).await
}
struct Event {
	method: String,
	params: Value,
}
impl Event {
	fn parse<E: MethodType + DeserializeOwned>(&self) -> Option<E> {
		(self.method == E::method_id()).then(|| serde_json::from_value(self.params.clone()).expect("chrome sends events of the protocol it speaks"))
	}
}

async fn run<T>(ws: Ws, headless: bool, close: bool, drops: &Path, target: impl AsyncFnOnce(&Cdp) -> Result<String>, work: impl AsyncFnOnce(&mut Tab) -> Result<T>) -> Result<T> {
	let (sink, stream) = ws.split();
	let cdp = Cdp {
		sink: AsyncMutex::new(sink),
		next_id: AtomicU64::new(1),
		pending: Mutex::default(),
	};
	let (tx, rx) = mpsc::unbounded_channel();
	let driver = pin!(cdp.drive(stream, tx));
	let window = Mutex::new(None);
	let run = pin!(async {
		let mut tab = Tab {
			cdp: &cdp,
			session: target(&cdp).await?,
			events: rx,
			login: None,
			headless,
			drops: drops.to_path_buf(),
			window: &window,
		};
		tab.call(page::EnableParams::default()).await?;
		tab.call(network::EnableParams::default()).await?;
		if headless {
			let ua = cdp.call(None, browser::GetVersionParams::default()).await?.user_agent;
			assert!(ua.contains("HeadlessChrome/"), "a headless chrome says so in its UA: {ua}");
			tab.call(emulation::SetUserAgentOverrideParams::new(ua.replace("HeadlessChrome/", "Chrome/"))).await?;
		}
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
	let run = pin!(async {
		match select(select(run, interrupted), follow).await {
			Either::Left((r, _)) => r.factor_first().0,
			Either::Right((e, _)) => e,
		}
	});
	let (r, driver) = match select(run, driver).await {
		Either::Left(done) => done,
		Either::Right((r, _)) => {
			r?;
			bail!("chrome closed the CDP connection mid-run");
		}
	};
	let window = window.lock().expect("never held across a panic").take();
	if let Some(mut w) = window {
		w.home()?;
	}
	if close {
		// the answer to `Browser.close` races the connection closing, so either ends it
		let close = pin!(cdp.call(None, browser::CloseParams::default()));
		match select(close, driver).await {
			Either::Left((c, _)) => c.map(|_| ())?,
			Either::Right((d, _)) => d?,
		}
	}
	r
}

struct Cdp {
	sink: AsyncMutex<SplitSink<Ws, Message>>,
	next_id: AtomicU64,
	pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>,
}
impl Cdp {
	async fn call<C: Command>(&self, session: Option<&str>, cmd: C) -> Result<C::Response> {
		let method = cmd.identifier();
		let id = self.next_id.fetch_add(1, Ordering::Relaxed);
		let (tx, rx) = oneshot::channel();
		self.pending.lock().expect("never held across a panic").insert(id, tx);
		let mut msg = serde_json::json!({ "id": id, "method": method, "params": serde_json::to_value(&cmd)? });
		if let Some(s) = session {
			msg["sessionId"] = s.into();
		}
		self.sink.lock().await.send(Message::text(msg.to_string())).await?;
		let result = rx
			.await
			.wrap_err_with(|| format!("chrome went away before answering {method}"))?
			.map_err(|e| eyre!("{method}: {e}"))?;
		C::response_from_value(result).wrap_err_with(|| format!("{method} answered in an unexpected shape"))
	}

	async fn attach(&self, target: TargetId) -> Result<String> {
		let mut attach = AttachToTargetParams::new(target);
		attach.flatten = Some(true);
		Ok(self.call(None, attach).await?.session_id.into())
	}

	/// Routes answers to their calls and everything else to `events`; resolves only when chrome hangs up.
	async fn drive(&self, mut stream: SplitStream<Ws>, events: mpsc::UnboundedSender<Event>) -> Result<()> {
		while let Some(msg) = stream.next().await {
			let Message::Text(text) = msg? else { continue };
			let mut v: Value = serde_json::from_str(&text)?;
			match v["id"].as_u64() {
				Some(id) => {
					let answer = match v.get("error") {
						Some(e) => Err(e.to_string()),
						None => Ok(v["result"].take()),
					};
					let tx = self.pending.lock().expect("never held across a panic").remove(&id).expect("chrome answers only what we asked");
					let _ = tx.send(answer); // the caller may have given up (timeout), and then the answer is moot
				}
				None => {
					let method = v["method"].as_str().expect("a message without an id is an event").to_string();
					let _ = events.send(Event { method, params: v["params"].take() }); // the tab is gone once the work is done
				}
			}
		}
		Ok(())
	}
}

/// `request`'s form body with its `cursor` variable set to `page`, when it is a request that pages
/// with one: facebook's pagination queries carry the cursor past the last page shown as a variable.
fn resume(request: &network::Request, page: &str) -> Result<Option<String>> {
	let query = request
		.headers
		.inner()
		.as_object()
		.and_then(|h| h.iter().find(|(k, _)| k.eq_ignore_ascii_case("x-fb-friendly-name")))
		.and_then(|(_, v)| v.as_str());
	if !query.is_some_and(|q| q.starts_with("Search")) {
		return Ok(None);
	}
	let Some(entries) = &request.post_data_entries else { return Ok(None) };
	let mut body = Vec::new();
	for entry in entries {
		let bytes = entry.bytes.as_ref().ok_or_else(|| eyre!("a post data entry of {} carries no bytes", request.url))?;
		body.extend(base64::engine::general_purpose::STANDARD.decode(AsRef::<str>::as_ref(bytes))?);
	}
	let mut form = reqwest::Url::parse("http://form/").expect("a constant base");
	form.set_query(Some(std::str::from_utf8(&body).wrap_err_with(|| format!("{} posted a body that is not form text", request.url))?));
	let mut pairs: Vec<(String, String)> = form.query_pairs().into_owned().collect();
	let Some((_, variables)) = pairs.iter_mut().find(|(k, _)| k == "variables") else {
		return Ok(None);
	};
	let mut parsed: Value = serde_json::from_str(variables).wrap_err("graphql `variables` is not JSON")?;
	let Some(cursor) = parsed.get_mut("cursor").filter(|c| c.is_string()) else { return Ok(None) };
	*cursor = page.into();
	*variables = parsed.to_string();
	form.query_pairs_mut().clear().extend_pairs(pairs);
	Ok(Some(form.query().expect("just set").to_string()))
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

/// Refuses a profile some chrome already holds: a second chrome on it hands its arguments over and exits.
async fn spawn(chrome: &Path, profile: &Path, headless: bool) -> Result<Ws> {
	let port_file = profile.join("DevToolsActivePort");
	if connect(&port_file).await.is_some() {
		bail!("a chrome is already running on {}; close it first", profile.display());
	}
	match std::fs::remove_file(&port_file) {
		Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
		_ => {}
	}
	std::process::Command::new(chrome)
		.arg(format!("--user-data-dir={}", profile.display()))
		.args(["--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check", "--window-size=1920,1080"])
		.args(headless.then_some("--headless=new"))
		.stdin(std::process::Stdio::null())
		.stdout(std::process::Stdio::null())
		.stderr(std::process::Stdio::null())
		.spawn()
		.wrap_err_with(|| format!("failed to start {}", chrome.display()))?;
	for _ in 0..300 {
		tokio::time::sleep(Duration::from_millis(100)).await;
		if let Some(ws) = connect(&port_file).await {
			return Ok(ws);
		}
	}
	bail!("{} never opened CDP on {}", chrome.display(), profile.display())
}

async fn connect(port_file: &Path) -> Option<Ws> {
	let port_file = std::fs::read_to_string(port_file).ok()?; // absent is chrome not (yet) running
	let (port, path) = port_file.split_once('\n')?; // chrome may be halfway through writing it
	// failure is a stale file left by a chrome that is gone
	tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}{}", path.trim())).await.ok().map(|(ws, _)| ws)
}
