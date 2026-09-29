//! `cargo r -p social_networks_adapters --example facebook_messenger -- <url> [--attached <user id>] [--send <text>] [<js expr>…]`
//!
//! Phase-0 spike on the burner's session: what a messenger page renders, whether its composer takes a
//! line headlessly, and whether a thread or the inbox is readable. With no exprs, a default set is
//! evaluated. `--send` sends for real.

use color_eyre::eyre::{Result, eyre};
use social_networks_adapters::{
	behaviour::{BehaviourConfig, Pace, ScrollPace},
	facebook::{self, AttachedConfig, FacebookConfig, LaunchedConfig},
};

const DEFAULT: [&str; 6] = [
	"location.href",
	r#"[...document.querySelectorAll('[role="textbox"][contenteditable="true"]')].map(e => [e.getAttribute('aria-label'), e.getAttribute('aria-disabled')])"#,
	"document.querySelector('[role=main]')?.innerText.slice(0, 3000) ?? document.body.innerText.slice(0, 3000)",
	r#"[...document.querySelectorAll('[role="row"]')].slice(-30).map(r => r.innerText)"#,
	r#"[...document.querySelectorAll('a[href*="/messages/"]')].slice(0, 40).map(a => [a.getAttribute('href'), a.innerText.slice(0, 80)])"#,
	r#"[...document.querySelectorAll('script[type="application/json"]')].map(s => s.textContent).filter(t => /message|thread/i.test(t)).map(t => t.length)"#,
];

#[tokio::main]
async fn main() -> Result<()> {
	let mut args = std::env::args().skip(1);
	let url = args
		.next()
		.ok_or_else(|| eyre!("usage: facebook_messenger <url> [--attached <user id>] [--send <text>] [<js expr>…]"))?;
	let mut rest: Vec<String> = args.collect();
	let attached = match rest.first().map(String::as_str) {
		Some("--attached") => {
			rest.remove(0);
			Some(rest.remove(0))
		}
		_ => None,
	};
	let send = match rest.first().map(String::as_str) {
		Some("--send") => {
			rest.remove(0);
			Some(rest.remove(0))
		}
		_ => None,
	};
	let exprs = match rest.is_empty() {
		true => DEFAULT.map(String::from).to_vec(),
		false => rest,
	};

	let behaviour = BehaviourConfig {
		active_hours: [0, 24],
		burst_min: 1e6,
		break_min: 1.,
		noise_share: 0.,
		load: Pace {
			per_hour: 150,
			per_day: 1500,
			dwell_secs: 2.,
			spread: 0.5,
		},
		scroll: ScrollPace {
			per_hour: 600,
			per_day: 4000,
			dwell_secs: 1.5,
			spread: 0.5,
			read_secs_per_item: 0.1,
		},
	};
	let config = FacebookConfig {
		attached: AttachedConfig {
			cdp_port: 49300,
			user_id: attached.clone().unwrap_or_default(),
			behaviour: behaviour.clone(),
		},
		launched: LaunchedConfig {
			chrome_executable: "/etc/profiles/per-user/v/bin/google-chrome-stable".into(),
			behaviour,
		},
	};
	let spike = async |fb: &mut facebook::Facebook<'_, '_>| fb.spike(&url, &exprs, send.as_deref()).await;
	let out = match attached {
		// a city venue is what routes to the attached session
		Some(_) => facebook::with_session(&config, &"facebook:city/0".parse()?, spike).await?,
		None => facebook::with_launched(&config, spike).await?,
	};
	for v in out {
		println!("{}\n---", serde_json::to_string_pretty(&v)?);
	}
	Ok(())
}
