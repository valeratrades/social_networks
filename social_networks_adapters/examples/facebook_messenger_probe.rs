//! What stands between the burner and its Messenger composer, typing nothing: opens the account's
//! conversation with itself, or with `--to` another's, and prints what `send` sees there (`messenger.js`), with a screenshot beside
//! the profile. `--through` then clicks through it the way `send` does, printing each step, until the
//! composer shows or something has no way past it.
//! `nix develop -c cargo r -p social_networks_adapters --example facebook_messenger_probe -- <chrome> <profile dir> [--through] [--to <profile id>]`

use std::{path::PathBuf, time::Duration};

use browser_manipulation::{Browser, Launch, Noise, Shot};
use color_eyre::eyre::{Result, bail, eyre};
use serde_json::Value;

const COMPOSER: &str = r#"[role="main"] [role="textbox"][contenteditable="true"][aria-label^="Write to "]"#;
const SEE: &str = include_str!("../src/facebook/messenger.js");

#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;
	let args: Vec<String> = std::env::args().skip(1).collect();
	let usage = "usage: facebook_messenger_probe <chrome> <profile dir> [--through] [--to <profile id>]";
	let [executable, profile, flags @ ..] = &args[..] else { bail!(usage) };
	let (executable, profile) = (PathBuf::from(executable), PathBuf::from(profile));
	let (through, to) = match flags {
		[] => (false, None),
		[t] if t == "--through" => (true, None),
		[o, id] if o == "--to" => (false, Some(id.clone())),
		[t, o, id] if t == "--through" && o == "--to" => (true, Some(id.clone())),
		_ => bail!(usage),
	};
	let shots = profile.parent().ok_or_else(|| eyre!("the profile dir has a parent"))?.to_path_buf();
	let browser = Browser::launch(
		Launch::Owned {
			profile,
			executable,
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
	tokio::time::sleep(Duration::from_secs(8)).await;
	let threads: Value = tab
		.eval(
			r#"() => [...document.querySelectorAll('a[href*="/messages/"]')].map(a => [a.getAttribute('href'), a.innerText.slice(0, 60)])"#,
			(),
		)
		.await?;
	println!("threads: {threads}");
	tab.goto(&format!("https://www.facebook.com/messages/t/{}", to.unwrap_or(me))).await?;
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
	tab.close().await?;
	browser.close().await?;
	Ok(())
}
