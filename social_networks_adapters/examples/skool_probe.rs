//! `SKOOL_EMAIL=… SKOOL_PASSWORD=… cargo r -p social_networks_adapters --example skool_probe -- <group> [route]`
//!
//! Answers the things about skool that cannot be checked without an account: that the login form
//! submits headlessly, that the cookies it leaves behind carry a plain `reqwest` request, and what
//! payload a member-visible route actually serves.
//!
//! `route` is appended under the group — `-/members`, `-/map` — and defaults to the feed. Every
//! `pageProps` key is printed, and the first entry of every list under one, which is how the shape
//! `Venue::members` and `Venue::posts` read is settled rather than guessed.

use color_eyre::eyre::{Result, eyre};
use social_networks_adapters::skool::{Skool, SkoolCredentials};

#[tokio::main]
async fn main() -> Result<()> {
	tracing_subscriber::fmt().with_env_filter("info").init();
	let group = std::env::args().nth(1).ok_or_else(|| eyre!("usage: skool_probe <group> [route]"))?;
	let route = std::env::args().nth(2);
	let creds = SkoolCredentials {
		email: std::env::var("SKOOL_EMAIL")?,
		password: std::env::var("SKOOL_PASSWORD")?,
	};

	let mut session = Skool::try_new(Some(creds))?;
	let path = match &route {
		Some(route) => format!("/{group}/{route}"),
		None => format!("/{group}"),
	};
	let payload = session.page(&path).await?;
	println!("served route: {}", payload["page"]);

	let props = payload.pointer("/props/pageProps").ok_or_else(|| eyre!("no pageProps on the served page"))?;
	let object = props.as_object().ok_or_else(|| eyre!("pageProps is not an object"))?;
	println!("pageProps keys: {:?}", object.keys().collect::<Vec<_>>());

	for (key, value) in object {
		match value {
			serde_json::Value::Array(list) =>
				if let Some(first) = list.first() {
					println!("\n{key}: {} entries, [0] = {first:#}", list.len());
				},
			// a scalar is the whole of what it says, and `dataUrl` is one
			serde_json::Value::Object(_) => {}
			scalar => println!("{key} = {scalar}"),
		}
	}
	Ok(())
}
