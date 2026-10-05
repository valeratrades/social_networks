//! What the burner's Messenger page is made of, without typing anything: opens the conversation with
//! the account itself and prints its editable boxes, whether it has a `main`, and its text.
//! `nix develop -c cargo r -p social_networks_adapters --example facebook_messenger_probe -- <chrome> <profile dir>`

use std::{path::PathBuf, time::Duration};

use browser_manipulation::{Browser, Launch, Robot};
use color_eyre::eyre::{Result, bail, eyre};

#[tokio::main]
async fn main() -> Result<()> {
	color_eyre::install()?;
	let mut args = std::env::args().skip(1);
	let (executable, profile) = match (args.next(), args.next()) {
		(Some(e), Some(p)) => (PathBuf::from(e), PathBuf::from(p)),
		_ => bail!("usage: facebook_messenger_probe <chrome> <profile dir>"),
	};
	let browser = Browser::launch(
		Launch::Owned {
			profile,
			executable,
			headless: true,
			viewport: None,
		},
		Robot,
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
	tab.goto(&format!("https://www.facebook.com/messages/t/{me}")).await?;
	tokio::time::sleep(Duration::from_secs(10)).await;
	let seen: serde_json::Value = tab
		.eval(
			r#"() => ({
				url: location.href,
				lang: document.documentElement.lang,
				main: !!document.querySelector('[role="main"]'),
				composer: document.querySelectorAll('[role="textbox"][contenteditable="true"][aria-label="Message"]').length,
				editable: [...document.querySelectorAll('[contenteditable="true"]')].map(e => ({ role: e.getAttribute('role'), label: e.getAttribute('aria-label'), text: e.textContent, in_main: !!e.closest('[role="main"]') })),
				main_text: document.querySelector('[role="main"]')?.innerText.slice(0, 1500),
			})"#,
			(),
		)
		.await?;
	println!("{}", serde_json::to_string_pretty(&seen)?);
	tab.close().await?;
	browser.close().await?;
	Ok(())
}
