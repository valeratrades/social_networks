//! What stands between the burner and its Messenger composer, typing nothing: opens the account's
//! conversation with itself, or with `--to` another's, and prints what `send` sees there (`messenger.js`), with a screenshot beside
//! the profile. `--through` then clicks through it the way `send` does, printing each step, until the
//! composer shows or something has no way past it, then closes the recipient suggestions and clicks
//! into the composer, as `send` does.
//! `--send <text>` instead sends `text`, split on blank lines into bubbles, through `send` itself; only
//! to the account itself or to [`OWN`].
//! `nix develop -c cargo r -p social_networks_adapters --example facebook_messenger_probe -- <chrome> <profile dir> [--through] [--to <profile id>] [--send <text>]`

use std::{path::PathBuf, time::Duration};

use browser_manipulation::{Browser, Launch, Noise, Shot};
use color_eyre::eyre::{Result, bail, ensure, eyre};
use serde_json::Value;
use social_networks_adapters::{
	facebook::{self, FacebookConfig},
	reach::Direct as _,
};

const COMPOSER: &str = r#"[role="main"] [role="textbox"][contenteditable="true"][aria-label^="Write to "]"#;
const SEE: &str = include_str!("../src/facebook/messenger.js");
/// the user's own account, in the burner's chat list
const OWN: &str = "100038744901538";

#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;
	let args: Vec<String> = std::env::args().skip(1).collect();
	let usage = "usage: facebook_messenger_probe <chrome> <profile dir> [--through] [--to <profile id>] [--send <text>]";
	let [executable, profile, flags @ ..] = &args[..] else { bail!(usage) };
	let mut flags = flags;
	let (executable, profile) = (PathBuf::from(executable), PathBuf::from(profile));
	let mut through = false;
	let mut to = None;
	let mut send = None;
	while !flags.is_empty() {
		flags = match flags {
			[t, rest @ ..] if t == "--through" => {
				through = true;
				rest
			}
			[o, id, rest @ ..] if o == "--to" => {
				to = Some(id.clone());
				rest
			}
			[s, text, rest @ ..] if s == "--send" => {
				send = Some(text.clone());
				rest
			}
			_ => bail!(usage),
		};
	}
	ensure!(
		send.is_none() || profile.parent().is_some_and(|d| d.ends_with("launched")),
		"`send` goes out from the launched session's chrome, not {}",
		profile.display()
	);
	let shots = profile.parent().ok_or_else(|| eyre!("the profile dir has a parent"))?.to_path_buf();
	let browser = Browser::launch(
		Launch::Owned {
			profile,
			executable: executable.clone(),
			headless: true,
			viewport: None,
		},
		// `send`'s pointer: a native click misses whichever of facebook's stacked twin buttons is underneath
		Noise::builder()
			.dwell(Duration::from_millis(250))
			.dwell_spread(0.5)
			.speed(1200.)
			.overshoot(0.15)
			.jitter(1.)
			.key_gap(Duration::from_millis(180))
			.key_spread(0.4)
			.typo(0.015)
			.notch(100.0..=120.)
			.notch_gap(Duration::from_millis(8)..=Duration::from_millis(40))
			.back(1. / 15.)
			.back_notches(1..=2)
			.seed(std::process::id().into())
			.build(),
		None,
	)
	.await?;
	let mut tab = browser.tab().await?;
	tab.goto("https://www.facebook.com/messages/").await?;
	let me = tab
		.cookies("https://www.facebook.com/")
		.await?
		.into_iter()
		.find(|c| c.name == "c_user")
		.ok_or_else(|| eyre!("the profile is logged out"))?
		.value;
	println!("the account: {me}");
	if let Some(text) = send {
		tab.close().await?;
		browser.close().await?;
		let to = match to {
			None => me,
			Some(to) if to == OWN => to,
			Some(to) => bail!("the probe sends only to the account itself or to {OWN}, not {to}"),
		};
		return sent(executable, &to, &text).await;
	}
	tokio::time::sleep(Duration::from_secs(8)).await;
	let threads: Value = tab
		.eval(
			r#"() => [...document.querySelectorAll('a[href*="/messages/"]')].map(a => [a.getAttribute('href'), a.innerText.slice(0, 60)])"#,
			(),
		)
		.await?;
	println!("threads: {threads}");
	tab.goto(&format!("https://www.facebook.com/messages/t/{}", to.as_deref().unwrap_or(&me))).await?;
	for step in 0..10 {
		tokio::time::sleep(Duration::from_secs(8)).await;
		let mut seen: Value = tab.eval(SEE, (COMPOSER, "probe")).await?;
		let text = seen["text"].take();
		let boxes: Value = tab
			.eval(
				r#"() => [...document.querySelectorAll('[contenteditable="true"], [role="textbox"], [role="combobox"], input:not([type="hidden"])')].map(e => ({ tag: e.tagName, role: e.getAttribute('role'), label: e.getAttribute('aria-label'), placeholder: e.getAttribute('placeholder'), in_main: !!e.closest('[role="main"]'), focused: e === document.activeElement }))"#,
				(),
			)
			.await?;
		println!("boxes: {boxes}");
		println!("step {step}: {}", serde_json::to_string_pretty(&seen)?);
		let shot = shots.join(format!("probe_{step}.png"));
		std::fs::write(&shot, tab.screenshot(Shot::Full).await?)?;
		println!("screenshot: {}", shot.display());
		let front = &seen["front"];
		let Some(pass) = front["pass"].as_str().filter(|_| through) else {
			if front.is_null() && seen["composers"] == 0 {
				println!("nothing in front and no composer; the page says: {}", text.as_str().expect("innerText"));
			}
			break;
		};
		let scope = match front["kind"].as_str() {
			Some("dialog") => r#":is([role="dialog"], [role="alertdialog"]):visible >> nth=-1"#,
			Some("notice") => r#"[role="main"]"#,
			other => bail!("a front of kind {other:?}"),
		};
		let button = format!("{scope} >> role=button[name=/^{}$/] >> nth=0", pass.replace('\'', "."));
		println!("clicking {button}");
		tab.click(&button).await?;
	}
	let after: Value = tab.eval(SEE, (COMPOSER, "probe")).await?;
	let log: String = tab.eval(r#"() => document.querySelector('[role="main"] [role="log"]')?.innerText ?? ''"#, ()).await?;
	println!("the conversation, as `main` lists it:\n{log}");
	if through && after["suggesting"] == true {
		const FOCUS: &str = r#"(composer) => { const e = document.activeElement; return { focused: !!e && e === document.querySelector(composer), focus: [e?.tagName, e?.getAttribute('role'), e?.getAttribute('aria-label')] } }"#;
		println!("closing the recipient suggestions with a click on `To:`, as `send` does");
		tab.click(r#"[role="main"] span:text-is("To:") >> nth=0"#).await?;
		tokio::time::sleep(Duration::from_secs(2)).await;
		let mut seen: Value = tab.eval(SEE, (COMPOSER, "probe")).await?;
		seen["text"].take();
		println!("after `To:`: {seen}");
		if seen["suggesting"] == false && seen["covered"] == false {
			println!("clicking into the composer, typing nothing");
			tab.click(COMPOSER).await?;
			tokio::time::sleep(Duration::from_secs(1)).await;
			let mut seen: Value = tab.eval(SEE, (COMPOSER, "probe")).await?;
			seen["text"].take();
			println!("in the composer: {seen}\nfocus: {}", tab.eval::<Value>(FOCUS, COMPOSER).await?);
		}
		let shot = shots.join("probe_focus.png");
		std::fs::write(&shot, tab.screenshot(Shot::Full).await?)?;
		println!("screenshot: {}", shot.display());
	}
	tab.close().await?;
	browser.close().await?;
	Ok(())
}

/// Through `Direct::send` on the launched session, bubble by bubble, as `purpose dm --multi-message` does.
async fn sent(chrome: PathBuf, to: &str, text: &str) -> Result<()> {
	let behaviour = serde_json::json!({
		"active_hours": [0, 24],
		"burst_min": 40,
		"break_min": 10,
		"noise_share": 0.05,
		"load": { "per_hour": 150, "per_day": 1500, "dwell_secs": 2, "spread": 0.5 },
		"scroll": { "per_hour": 600, "per_day": 4000, "dwell_secs": 1.5, "spread": 0.5, "read_secs_per_item": 0.1 },
	});
	let config: FacebookConfig = serde_json::from_value(serde_json::json!({
		"attached": { "cdp_port": 0, "user_id": "", "behaviour": behaviour },
		"launched": { "chrome_executable": chrome, "behaviour": behaviour },
	}))?;
	facebook::with_sender(&config, async |fb| {
		for bubble in text.split("\n\n").map(str::trim) {
			fb.send(to, bubble).await?;
			println!("sent to {to}: {bubble}");
		}
		Ok(())
	})
	.await
}
